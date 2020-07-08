// Copyright (C) 2019-2020  Pierre Krieger
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use super::{ExecOutcome, GlobalValueErr, NewErr, RunErr, Signature, StartErr, WasmValue};

use alloc::{boxed::Box, rc::Rc, vec::Vec};
use core::{cell::RefCell, convert::TryFrom, fmt};

mod coroutine;

/// Prototype for a [`Jit`].
pub struct JitPrototype {
    /// Coroutine that contains the Wasm execution stack.
    coroutine: coroutine::Coroutine<
        Box<dyn FnOnce() -> Result<Option<wasmtime::Val>, wasmtime::Trap>>,
        FromCoroutine,
        ToCoroutine,
    >,

    /// Reference to the memory imported by the module, if any.
    imported_memory: Option<wasmtime::Memory>,
}

impl JitPrototype {
    /// Creates a new process state machine from the given module.
    ///
    /// The closure is called for each import that the module has. It must assign a number to each
    /// import, or return an error if the import can't be resolved. When the VM calls one of these
    /// functions, this number will be returned back in order for the user to know how to handle
    /// the call.
    pub fn new(
        module: &WasmBlob,
        mut symbols: impl FnMut(&str, &str, &Signature) -> Result<usize, ()>,
    ) -> Result<Self, NewErr> {
        let engine = wasmtime::Engine::new(&Default::default());
        let store = wasmtime::Store::new(&engine);
        let module = wasmtime::Module::from_binary(&store, &module.bytes).unwrap();

        let builder = coroutine::CoroutineBuilder::new();

        let mut imported_memory = None;

        // Building the list of symbols that the Wasm VM is able to use.
        let imports = {
            let mut imports = Vec::with_capacity(module.imports().len());
            for import in module.imports() {
                match import.ty() {
                    wasmtime::ExternType::Func(f) => {
                        // TODO: don't panic if not found
                        let function_index =
                            symbols(import.module(), import.name(), &From::from(f)).unwrap();
                        let interrupter = builder.interrupter();
                        imports.push(wasmtime::Extern::Func(wasmtime::Func::new(
                            &store,
                            f.clone(),
                            move |_, params, ret_val| {
                                // This closure is executed whenever the Wasm VM calls an external function.
                                let returned = interrupter.interrupt(FromCoroutine::Interrupt {
                                    function_index,
                                    parameters: params.iter().cloned().map(From::from).collect(),
                                });
                                let returned = match returned {
                                    ToCoroutine::Resume(returned) => returned,
                                    _ => unreachable!(),
                                };
                                if let Some(returned) = returned {
                                    assert_eq!(ret_val.len(), 1);
                                    ret_val[0] = From::from(returned);
                                } else {
                                    assert!(ret_val.is_empty());
                                }
                                Ok(())
                            },
                        )));
                    }
                    wasmtime::ExternType::Global(_) => unimplemented!(),
                    wasmtime::ExternType::Table(_) => unimplemented!(),
                    wasmtime::ExternType::Memory(m) => {
                        // TODO: check name and all?
                        // TODO: proper error instead of asserting?
                        assert!(imported_memory.is_none());
                        imported_memory = Some(wasmtime::Memory::new(
                            &store,
                            wasmtime::MemoryType::new(m.limits().clone()),
                        ));
                        imports.push(wasmtime::Extern::Memory(
                            imported_memory.as_ref().unwrap().clone(),
                        ));
                    }
                };
            }
            imports
        };

        // We now build the coroutine of the main thread.
        let mut coroutine = {
            let interrupter = builder.interrupter();
            builder.build(Box::new(move || {
                // TODO: don't unwrap
                let instance = wasmtime::Instance::new(&module, &imports).unwrap();

                let memory = if let Some(mem) = instance.get_export("memory") {
                    if let Some(mem) = mem.memory() {
                        Some(mem.clone())
                    } else {
                        let err = NewErr::MemoryIsntMemory;
                        interrupter.interrupt(FromCoroutine::Init(Err(err)));
                        return Ok(None);
                    }
                } else {
                    None
                };

                let indirect_table =
                    if let Some(tbl) = instance.get_export("__indirect_function_table") {
                        if let Some(tbl) = tbl.table() {
                            Some(tbl.clone())
                        } else {
                            let err = NewErr::IndirectTableIsntTable;
                            interrupter.interrupt(FromCoroutine::Init(Err(err)));
                            return Ok(None);
                        }
                    } else {
                        None
                    };

                let mut request = interrupter.interrupt(FromCoroutine::Init(Ok(())));
                let start_function_name = loop {
                    match request {
                        ToCoroutine::Start(n) => break n,
                        ToCoroutine::GetGlobal(global) => {
                            let global_val = match instance.get_export(&global) {
                                Some(wasmtime::Extern::Global(g)) => match g.get() {
                                    wasmtime::Val::I32(v) => {
                                        Ok(u32::from_ne_bytes(v.to_ne_bytes()))
                                    }
                                    _ => Err(GlobalValueErr::Invalid),
                                },
                                _ => Err(GlobalValueErr::NotFound),
                            };

                            request =
                                interrupter.interrupt(FromCoroutine::GetGlobalResponse(global_val));
                        }
                        ToCoroutine::GetMemoryTable => {
                            request =
                                interrupter.interrupt(FromCoroutine::GetMemoryTableResponse {
                                    memory: memory.clone(),
                                    indirect_table: indirect_table.clone(),
                                });
                        }
                        // TODO:
                        _ => todo!(),
                    }
                };

                // Try to start executing `_start`.
                let start_function = if let Some(f) = instance.get_export(&start_function_name) {
                    if let Some(f) = f.func() {
                        f.clone()
                    } else {
                        let err = NewErr::NotAFunction;
                        interrupter.interrupt(FromCoroutine::Init(Err(err)));
                        return Ok(None);
                    }
                } else {
                    let err = NewErr::FunctionNotFound;
                    interrupter.interrupt(FromCoroutine::Init(Err(err)));
                    return Ok(None);
                };

                // Report back that everything went ok.
                let reinjected: ToCoroutine = interrupter.interrupt(FromCoroutine::Init(Ok(())));
                assert!(matches!(reinjected, ToCoroutine::Resume(None)));

                // Now running the `start` function of the Wasm code.
                // This will interrupt the coroutine every time we reach an external function.
                let result = start_function.call(&[])?;

                // Execution resumes here when the Wasm code has gracefully finished.
                assert!(result.len() == 0 || result.len() == 1); // TODO: I don't know what multiple results means
                if result.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(result[0].clone())) // TODO: don't clone?
                }
            }) as Box<_>)
        };

        // Execute the coroutine once, as described above.
        // The first yield must always be an `FromCoroutine::Init`.
        match coroutine.run(None) {
            coroutine::RunOut::Interrupted(FromCoroutine::Init(Err(err))) => return Err(err),
            coroutine::RunOut::Interrupted(FromCoroutine::Init(Ok(()))) => {}
            _ => unreachable!(),
        }

        Ok(JitPrototype {
            coroutine,
            imported_memory,
        })
    }

    /// Returns the value of a global that the module exports.
    pub fn global_value(&mut self, name: &str) -> Result<u32, GlobalValueErr> {
        match self
            .coroutine
            .run(Some(ToCoroutine::GetGlobal(name.to_owned())))
        {
            coroutine::RunOut::Interrupted(FromCoroutine::GetGlobalResponse(outcome)) => outcome,
            _ => unreachable!(),
        }
    }

    /// Turns this prototype into an actual virtual machine. This requires choosing which function
    /// to execute.
    pub fn start(mut self, function_name: &str, params: &[WasmValue]) -> Result<Jit, NewErr> {
        let (exported_memory, indirect_table) =
            match self.coroutine.run(Some(ToCoroutine::GetMemoryTable)) {
                coroutine::RunOut::Interrupted(FromCoroutine::GetMemoryTableResponse {
                    memory,
                    indirect_table,
                }) => (memory, indirect_table),
                _ => unreachable!(),
            };

        match self
            .coroutine
            .run(Some(ToCoroutine::Start(function_name.to_owned())))
        {
            coroutine::RunOut::Interrupted(FromCoroutine::Init(Err(err))) => return Err(err),
            coroutine::RunOut::Interrupted(FromCoroutine::Init(Ok(()))) => {}
            _ => unreachable!(),
        }

        // TODO: proper error instead of panicking?
        let memory = match (exported_memory, self.imported_memory) {
            (Some(_), Some(_)) => unimplemented!(),
            (Some(m), None) => Some(m),
            (None, Some(m)) => Some(m),
            (None, None) => None,
        };

        Ok(Jit {
            coroutine: self.coroutine,
            memory,
            indirect_table,
        })
    }
}

/// Type that can be given to the coroutine.
enum ToCoroutine {
    /// Start execution of the given function. Answered with [`FromCoroutine::Init`].
    Start(String),
    /// Resume execution after [`FromCoroutine::Interrupt`].
    Resume(Option<WasmValue>),
    /// Return the memory and indirect table globals.
    GetMemoryTable,
    /// Return the value of the given global with a [`FromCoroutine::GetGlobalResponse`].
    GetGlobal(String),
}

/// Type yielded by the coroutine.
enum FromCoroutine {
    /// Reports how well the initialization went. Sent as part of the first interrupt, then again
    /// as a reponse to [`ToCoroutine::Start`].
    Init(Result<(), NewErr>),
    /// Execution of the Wasm code has been interrupted by a call.
    Interrupt {
        /// Index of the function, to put in [`ExecOutcome::Interrupted::id`].
        function_index: usize,
        /// Parameters of the function.
        parameters: Vec<WasmValue>,
    },
    /// Response to a [`ToCoroutine::GetMemoryTable`].
    GetMemoryTableResponse {
        memory: Option<wasmtime::Memory>,
        indirect_table: Option<wasmtime::Table>,
    },
    /// Response to a [`ToCoroutine::GetGlobal`].
    GetGlobalResponse(Result<u32, GlobalValueErr>),
}

/// Wasm VM that uses JITted compilation.
pub struct Jit {
    /// Coroutine that contains the Wasm execution stack.
    coroutine: coroutine::Coroutine<
        Box<dyn FnOnce() -> Result<Option<wasmtime::Val>, wasmtime::Trap>>,
        FromCoroutine,
        ToCoroutine,
    >,

    /// Reference to the memory, in case we need to access it.
    /// `None` if the module doesn't export its memory.
    memory: Option<wasmtime::Memory>,

    /// Reference to the table of indirect functions, in case we need to access it.
    /// `None` if the module doesn't export such table.
    indirect_table: Option<wasmtime::Table>,
}

impl Jit {
    /// Returns true if the state machine is in a poisoned state and cannot run anymore.
    pub fn is_poisoned(&self) -> bool {
        self.coroutine.is_finished()
    }

    /// Starts or continues execution of this thread.
    ///
    /// If this is the first call you call [`run`](Thread::run) for this thread, then you must pass
    /// a value of `None`.
    /// If, however, you call this function after a previous call to [`run`](Thread::run) that was
    /// interrupted by an external function call, then you must pass back the outcome of that call.
    pub fn run(&mut self, value: Option<WasmValue>) -> Result<ExecOutcome, RunErr> {
        if self.coroutine.is_finished() {
            return Err(RunErr::Poisoned);
        }

        // TODO: check value type

        // Resume the coroutine execution.
        match self
            .coroutine
            .run(Some(ToCoroutine::Resume(value.map(From::from))))
        {
            coroutine::RunOut::Finished(Err(err)) => {
                // TODO: don't println
                println!("err: {}", err);
                Ok(ExecOutcome::Finished {
                    return_value: Err(()),
                })
            }
            coroutine::RunOut::Finished(Ok(val)) => Ok(ExecOutcome::Finished {
                return_value: Ok(val.map(From::from)),
            }),
            coroutine::RunOut::Interrupted(FromCoroutine::Interrupt {
                function_index,
                parameters,
            }) => Ok(ExecOutcome::Interrupted {
                id: function_index,
                params: parameters,
            }),

            // `Init` must only be produced at initialization.
            coroutine::RunOut::Interrupted(FromCoroutine::Init(_)) => unreachable!(),
            // `GetGlobalResponse` only happens in response to a request.
            coroutine::RunOut::Interrupted(FromCoroutine::GetGlobalResponse(_)) => unreachable!(),
            // `GetMemoryTableResponse` only happens in response to a request.
            coroutine::RunOut::Interrupted(FromCoroutine::GetMemoryTableResponse { .. }) => {
                unreachable!()
            }
        }
    }

    /// Returns the size of the memory, in bytes.
    ///
    /// > **Note**: This can change over time if the Wasm code uses the `grow` opcode.
    pub fn memory_size(&self) -> u32 {
        let mem = match self.memory.as_ref() {
            Some(m) => m,
            None => return 0,
        };

        u32::try_from(mem.data_size()).unwrap()
    }

    /// Copies the given memory range into a `Vec<u8>`.
    ///
    /// Returns an error if the range is invalid or out of range.
    pub fn read_memory(&self, offset: u32, size: u32) -> Result<Vec<u8>, ()> {
        let mem = self.memory.as_ref().ok_or(())?;
        let start = usize::try_from(offset).map_err(|_| ())?;
        let end = start
            .checked_add(usize::try_from(size).map_err(|_| ())?)
            .ok_or(())?;

        // Soundness: the documentation of wasmtime precisely explains what is safe or not.
        // Basically, we are safe as long as we are sure that we don't potentially grow the
        // buffer (which would invalidate the buffer pointer).
        unsafe { Ok(mem.data_unchecked()[start..end].to_vec()) }
    }

    /// Write the data at the given memory location.
    ///
    /// Returns an error if the range is invalid or out of range.
    pub fn write_memory(&mut self, offset: u32, value: &[u8]) -> Result<(), ()> {
        let mem = self.memory.as_ref().ok_or(())?;
        let start = usize::try_from(offset).map_err(|_| ())?;
        let end = start.checked_add(value.len()).ok_or(())?;

        // Soundness: the documentation of wasmtime precisely explains what is safe or not.
        // Basically, we are safe as long as we are sure that we don't potentially grow the
        // buffer (which would invalidate the buffer pointer).
        unsafe {
            mem.data_unchecked_mut()[start..end].copy_from_slice(value);
        }

        Ok(())
    }
}

// TODO: explain how this is sound
unsafe impl Send for Jit {}

impl fmt::Debug for Jit {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Jit").finish()
    }
}

/// Wasm blob known to be valid.
// Note: this struct exists in order to hide wasmtime as an implementation detail.
pub struct WasmBlob {
    // TODO: do something better than that?
    bytes: Vec<u8>,
}

impl WasmBlob {
    // TODO: better error type
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> Result<Self, ()> {
        Ok(WasmBlob {
            bytes: bytes.as_ref().to_owned(),
        })
    }
}

impl<'a> TryFrom<&'a [u8]> for WasmBlob {
    type Error = (); // TODO: better error type

    fn try_from(bytes: &'a [u8]) -> Result<Self, Self::Error> {
        WasmBlob::from_bytes(bytes)
    }
}
