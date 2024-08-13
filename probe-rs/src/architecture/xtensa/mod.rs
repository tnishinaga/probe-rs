//! All the interface bits for Xtensa.

use std::{sync::Arc, time::Duration};

use probe_rs_target::{Architecture, CoreType, InstructionSet};

use crate::{
    architecture::xtensa::{
        arch::{instruction::Instruction, Register, SpecialRegister},
        communication_interface::{DebugCause, IBreakEn, XtensaCommunicationInterface},
        registers::{FP, PC, RA, SP, XTENSA_CORE_REGSISTERS},
        sequences::XtensaDebugSequence,
    },
    core::{
        registers::{CoreRegisters, RegisterId, RegisterValue},
        BreakpointCause,
    },
    memory::CoreMemoryInterface,
    semihosting::decode_semihosting_syscall,
    CoreInformation, CoreInterface, CoreRegister, CoreStatus, Error, HaltReason, MemoryInterface,
    SemihostingCommand,
};

pub(crate) mod arch;
pub(crate) mod xdm;

pub mod communication_interface;
pub(crate) mod registers;
pub(crate) mod sequences;

#[derive(Debug)]
/// Xtensa core state.
pub struct XtensaCoreState {
    /// Whether the core is enabled.
    enabled: bool,

    /// Whether hardware breakpoints are enabled.
    breakpoints_enabled: bool,

    /// Whether each hardware breakpoint is set.
    // 2 is the architectural upper limit. The actual count is stored in
    // [`communication_interface::XtensaInterfaceState`]
    breakpoint_set: [bool; 2],

    /// Whether the PC was written since we last halted. Used to avoid incrementing the PC on
    /// resume.
    pc_written: bool,

    /// The semihosting command that was decoded at the current program counter
    semihosting_command: Option<SemihostingCommand>,
}

impl XtensaCoreState {
    /// Creates a new [`XtensaCoreState`].
    pub(crate) fn new() -> Self {
        Self {
            enabled: false,
            breakpoints_enabled: false,
            breakpoint_set: [false; 2],
            pc_written: false,
            semihosting_command: None,
        }
    }

    /// Creates a bitmask of the currently set breakpoints.
    fn breakpoint_mask(&self) -> u32 {
        self.breakpoint_set
            .iter()
            .enumerate()
            .fold(0, |acc, (i, &set)| if set { acc | (1 << i) } else { acc })
    }
}

/// An interface to operate an Xtensa core.
pub struct Xtensa<'probe> {
    interface: XtensaCommunicationInterface<'probe>,
    state: &'probe mut XtensaCoreState,
    sequence: Arc<dyn XtensaDebugSequence>,
}

impl<'probe> Xtensa<'probe> {
    const IBREAKA_REGS: [SpecialRegister; 2] =
        [SpecialRegister::IBreakA0, SpecialRegister::IBreakA1];

    /// Create a new Xtensa interface for a particular core.
    pub fn new(
        interface: XtensaCommunicationInterface<'probe>,
        state: &'probe mut XtensaCoreState,
        sequence: Arc<dyn XtensaDebugSequence>,
    ) -> Result<Self, Error> {
        let mut this = Self {
            interface,
            state,
            sequence,
        };

        this.on_attach()?;

        Ok(this)
    }

    fn on_attach(&mut self) -> Result<(), Error> {
        // If the core was reset, force a reconnection.
        let core_reset;
        if self.state.enabled {
            core_reset = self.interface.xdm.core_was_reset()?;
            let debug_reset = self.interface.xdm.debug_was_reset()?;

            if core_reset {
                tracing::debug!("Core was reset");
                *self.state = XtensaCoreState::new();
            }
            if debug_reset {
                tracing::debug!("Debug was reset");
                self.state.enabled = false;
            }
        } else {
            core_reset = true;
        }

        // (Re)enter debug mode if necessary. This also checks if the core is enabled.
        if !self.state.enabled {
            // Enable debug module.
            self.interface.enter_debug_mode()?;
            self.state.enabled = true;

            if core_reset {
                // Run the connection sequence while halted.
                let was_halted = self.core_halted()?;
                if !was_halted {
                    self.halt(Duration::from_millis(500))?;
                }

                self.sequence.on_connect(&mut self.interface)?;

                if !was_halted {
                    self.run()?;
                }
            }
        }

        Ok(())
    }

    fn core_info(&mut self) -> Result<CoreInformation, Error> {
        let pc = self.read_core_reg(self.program_counter().id)?;

        Ok(CoreInformation { pc: pc.try_into()? })
    }

    fn skip_breakpoint_instruction(&mut self) -> Result<(), Error> {
        self.state.semihosting_command = None;
        if !self.state.pc_written {
            let debug_cause = self.interface.read_register::<DebugCause>()?;

            let pc_increment = if debug_cause.break_instruction() {
                3
            } else if debug_cause.break_n_instruction() {
                2
            } else {
                0
            };

            if pc_increment > 0 {
                // Step through the breakpoint
                let mut pc = self.read_core_reg(self.program_counter().id)?;

                pc.increment_address(pc_increment)?;

                self.write_core_reg(self.program_counter().into(), pc)?;
            }
        }

        Ok(())
    }

    /// Check if the current breakpoint is a semihosting call
    // OpenOCD implementation: https://github.com/espressif/openocd-esp32/blob/93dd01511fd13d4a9fb322cd9b600c337becef9e/src/target/espressif/esp_xtensa_semihosting.c#L42-L103
    fn check_for_semihosting(&mut self) -> Result<Option<SemihostingCommand>, Error> {
        let pc: u32 = self.read_core_reg(self.program_counter().id)?.try_into()?;

        let mut actual_instructions = [0u8; 3];
        self.read_8((pc) as u64, &mut actual_instructions)?;

        tracing::debug!(
            "Semihosting check pc={pc:#x} instructions={0:#08x} {1:#08x} {2:#08x}",
            actual_instructions[0],
            actual_instructions[1],
            actual_instructions[2],
        );

        let expected_instruction = Instruction::Break(1, 14).to_bytes();

        tracing::debug!(
            "Expected instructions={0:#08x} {1:#08x} {2:#08x}",
            expected_instruction[0],
            expected_instruction[1],
            expected_instruction[2]
        );

        if actual_instructions[..3] == expected_instruction.as_slice()[..3] {
            match self.state.semihosting_command {
                None => {
                    // We only want to decode the semihosting command once, since answering it might change some of the registers
                    let a2: u32 = self.read_core_reg(RegisterId::from(2))?.try_into()?;
                    let a3: u32 = self.read_core_reg(RegisterId::from(3))?.try_into()?;

                    tracing::info!("Semihosting found pc={pc:#x} a2={a2:#x} a3={a3:#x}");
                    let cmd = decode_semihosting_syscall(self, a2, a3)?;
                    self.state.semihosting_command = Some(cmd);
                    Ok(Some(cmd))
                }
                Some(command) => Ok(Some(command)),
            }
        } else {
            Ok(None)
        }
    }
}

impl<'probe> CoreMemoryInterface for Xtensa<'probe> {
    type ErrorType = Error;

    fn memory(&self) -> &dyn MemoryInterface<Self::ErrorType> {
        &self.interface
    }

    fn memory_mut(&mut self) -> &mut dyn MemoryInterface<Self::ErrorType> {
        &mut self.interface
    }
}

impl<'probe> CoreInterface for Xtensa<'probe> {
    fn wait_for_core_halted(&mut self, timeout: Duration) -> Result<(), Error> {
        self.interface.wait_for_core_halted(timeout)?;
        self.state.pc_written = false;

        let status = self.status()?;

        tracing::debug!("Core halted: {:#?}", status);

        Ok(())
    }

    fn core_halted(&mut self) -> Result<bool, Error> {
        Ok(self.interface.core_halted()?)
    }

    fn status(&mut self) -> Result<CoreStatus, Error> {
        let status = if self.core_halted()? {
            let debug_cause = self.interface.read_register::<DebugCause>()?;
            let reason =
                if debug_cause.halt_reason() == HaltReason::Breakpoint(BreakpointCause::Software) {
                    // The chip initiated this halt, therefore we need to update pc_written state
                    self.state.pc_written = false;
                    // Check if the breakpoint is a semihosting call
                    if let Some(cmd) = self.check_for_semihosting()? {
                        HaltReason::Breakpoint(BreakpointCause::Semihosting(cmd))
                    } else {
                        debug_cause.halt_reason()
                    }
                } else {
                    debug_cause.halt_reason()
                };
            CoreStatus::Halted(reason)
        } else {
            CoreStatus::Running
        };

        Ok(status)
    }

    fn halt(&mut self, timeout: Duration) -> Result<CoreInformation, Error> {
        self.interface.halt()?;
        self.interface.wait_for_core_halted(timeout)?;

        self.core_info()
    }

    fn run(&mut self) -> Result<(), Error> {
        self.skip_breakpoint_instruction()?;
        if self.state.pc_written {
            self.interface.clear_register_cache();
        }
        Ok(self.interface.resume()?)
    }

    fn reset(&mut self) -> Result<(), Error> {
        self.reset_and_halt(Duration::from_millis(500))?;

        self.run()
    }

    fn reset_and_halt(&mut self, timeout: Duration) -> Result<CoreInformation, Error> {
        self.state.semihosting_command = None;
        self.sequence
            .reset_system_and_halt(&mut self.interface, timeout)?;

        // TODO: this may return that the core has gone away, which is fine but currently unexpected
        self.on_attach()?;

        self.core_info()
    }

    fn step(&mut self) -> Result<CoreInformation, Error> {
        self.skip_breakpoint_instruction()?;
        self.interface.step()?;
        self.state.pc_written = false;

        self.core_info()
    }

    fn read_core_reg(&mut self, address: RegisterId) -> Result<RegisterValue, Error> {
        let register = Register::try_from(address)?;
        let value = self.interface.read_register_untyped(register)?;

        Ok(RegisterValue::U32(value))
    }

    fn write_core_reg(&mut self, address: RegisterId, value: RegisterValue) -> Result<(), Error> {
        let value: u32 = value.try_into()?;

        if address == self.program_counter().id {
            self.state.pc_written = true;
        }

        let register = Register::try_from(address)?;
        self.interface.write_register_untyped(register, value)?;

        Ok(())
    }

    fn available_breakpoint_units(&mut self) -> Result<u32, Error> {
        Ok(self.interface.available_breakpoint_units())
    }

    fn hw_breakpoints(&mut self) -> Result<Vec<Option<u64>>, Error> {
        let mut breakpoints = Vec::with_capacity(self.available_breakpoint_units()? as usize);

        let enabled_breakpoints = self.interface.read_register::<IBreakEn>()?;

        for i in 0..self.available_breakpoint_units()? as usize {
            let is_enabled = enabled_breakpoints.0 & (1 << i) != 0;
            let breakpoint = if is_enabled {
                let address = self
                    .interface
                    .read_register_untyped(Self::IBREAKA_REGS[i])?;

                Some(address as u64)
            } else {
                None
            };

            breakpoints.push(breakpoint);
        }

        Ok(breakpoints)
    }

    fn enable_breakpoints(&mut self, state: bool) -> Result<(), Error> {
        self.state.breakpoints_enabled = state;
        let mask = if state {
            self.state.breakpoint_mask()
        } else {
            0
        };

        self.interface.write_register(IBreakEn(mask))?;

        Ok(())
    }

    fn set_hw_breakpoint(&mut self, unit_index: usize, addr: u64) -> Result<(), Error> {
        self.state.breakpoint_set[unit_index] = true;
        self.interface
            .write_register_untyped(Self::IBREAKA_REGS[unit_index], addr as u32)?;

        if self.state.breakpoints_enabled {
            let mask = self.state.breakpoint_mask();
            self.interface.write_register(IBreakEn(mask))?;
        }

        Ok(())
    }

    fn clear_hw_breakpoint(&mut self, unit_index: usize) -> Result<(), Error> {
        self.state.breakpoint_set[unit_index] = false;

        if self.state.breakpoints_enabled {
            let mask = self.state.breakpoint_mask();
            self.interface.write_register(IBreakEn(mask))?;
        }

        Ok(())
    }

    fn registers(&self) -> &'static CoreRegisters {
        &XTENSA_CORE_REGSISTERS
    }

    fn program_counter(&self) -> &'static CoreRegister {
        &PC
    }

    fn frame_pointer(&self) -> &'static CoreRegister {
        &FP
    }

    fn stack_pointer(&self) -> &'static CoreRegister {
        &SP
    }

    fn return_address(&self) -> &'static CoreRegister {
        &RA
    }

    fn hw_breakpoints_enabled(&self) -> bool {
        self.state.breakpoints_enabled
    }

    fn architecture(&self) -> Architecture {
        Architecture::Xtensa
    }

    fn core_type(&self) -> CoreType {
        CoreType::Xtensa
    }

    fn instruction_set(&mut self) -> Result<InstructionSet, Error> {
        // TODO: NX exists, too
        Ok(InstructionSet::Xtensa)
    }

    fn fpu_support(&mut self) -> Result<bool, Error> {
        // TODO: ESP32 and ESP32-S3 have FPU
        Ok(false)
    }

    fn floating_point_register_count(&mut self) -> Result<usize, Error> {
        // TODO: ESP32 and ESP32-S3 have FPU
        Ok(0)
    }

    fn reset_catch_set(&mut self) -> Result<(), Error> {
        Err(Error::NotImplemented("reset_catch_set"))
    }

    fn reset_catch_clear(&mut self) -> Result<(), Error> {
        Err(Error::NotImplemented("reset_catch_clear"))
    }

    fn debug_core_stop(&mut self) -> Result<(), Error> {
        self.interface.leave_debug_mode()?;
        Ok(())
    }
}
