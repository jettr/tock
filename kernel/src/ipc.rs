//! Inter-process communication mechanism for Tock.
//!
//! This is a special syscall driver that allows userspace applications to
//! share memory.

use crate::capabilities::MemoryAllocationCapability;
use crate::grant::{AllowRoCount, AllowRwCount, Grant, UpcallCount};
use crate::kernel::Kernel;
use crate::process;
use crate::process::ProcessId;
use crate::processbuffer::ReadableProcessBuffer;
use crate::syscall_driver::{CommandReturn, SyscallDriver};
use crate::ErrorCode;

/// Syscall number
pub const DRIVER_NUM: usize = 0x10000;

/// Ids for read-only allow buffers
mod ro_allow {
    pub(super) const SEARCH: usize = 0;
    /// The number of allow buffers the kernel stores for this grant
    pub(super) const COUNT: usize = 1;
}

/// Enum to mark which type of upcall is scheduled for the IPC mechanism.
#[derive(Copy, Clone, Debug)]
pub enum IPCUpcallType {
    /// Indicates that the upcall is for the service upcall handler this
    /// process has setup.
    Service,
    /// Indicates that the upcall is from a different service app and will
    /// call one of the client upcalls setup by this process.
    Client,
}

/// State that is stored in each process's grant region to support IPC.
#[derive(Default)]
struct IPCData;

/// The upcall setup by a service. Each process can only be one service.
/// Subscribe with subscribe_num == 0 is how a process registers
/// itself as an IPC service. Each process can only register as a
/// single IPC service. The identifier for the IPC service is the
/// application name stored in the TBF header of the application.
/// The upcall that is passed to subscribe is called when another
/// process notifies the server process.
const SERVICE_UPCALL_NUM: usize = 0;

/// This const specifies the subscribe_num of the first upcall
/// in the array of client upcalls.
/// Subscribe with subscribe_num >= 1 is how a client registers
/// a upcall for a given service. The service number (passed
/// as subscribe_num) is returned from the allow() call.
/// Once subscribed, the client will receive upcalls when the
/// service process calls notify_client().

const CLIENT_UPCALL_NUM_BASE: usize = 1;

/// The IPC mechanism struct.
/// NUM_UPCALLS should always equal NUM_PROCS + 1. The extra upcall
/// is so processes can register as a service. Once const_evaluatable_checked
/// is stable we will not need two separate const generic parameters.
pub struct IPC<const NUM_PROCS: usize, const NUM_UPCALLS: usize> {
    /// The grant regions for each process that holds the per-process IPC data.
    data: Grant<
        IPCData,
        UpcallCount<NUM_UPCALLS>,
        AllowRoCount<{ ro_allow::COUNT }>,
        AllowRwCount<NUM_PROCS>,
    >,
}

impl<const NUM_PROCS: usize, const NUM_UPCALLS: usize> IPC<NUM_PROCS, NUM_UPCALLS> {
    pub fn new(
        kernel: &'static Kernel,
        driver_num: usize,
        capability: &dyn MemoryAllocationCapability,
    ) -> Self {
        Self {
            data: kernel.create_grant(driver_num, capability),
        }
    }

    /// Schedule an IPC upcall for a process. This is called by the main
    /// scheduler loop if an IPC task was queued for the process.
    pub(crate) unsafe fn schedule_upcall(
        &self,
        schedule_on: ProcessId,
        called_from: ProcessId,
        cb_type: IPCUpcallType,
    ) -> Result<(), process::Error> {
        self.data
            .enter(schedule_on, |_, schedule_on_kernel_data| {
                let to_schedule: usize = match cb_type {
                    IPCUpcallType::Service => SERVICE_UPCALL_NUM,
                    IPCUpcallType::Client => match called_from.index() {
                        Some(i) => i + CLIENT_UPCALL_NUM_BASE,
                        None => panic!("Invalid app issued IPC request"), //TODO: return Error instead
                    },
                };
                self.data.enter(called_from, |_, called_from_kernel_data| {
                    // If the other app shared a buffer with us, make
                    // sure we have access to that slice and then call
                    // the upcall. If no slice was shared then just
                    // call the upcall.
                    if let Some(i) = schedule_on.index() {
                        let called_from_id = match called_from.index() {
                            Some(index) => index,
                            // If index is invalid, then we cannot notify
                            None => return,
                        };
                        match called_from_kernel_data.get_readwrite_processbuffer(i) {
                            Ok(slice) => {
                                self.data
                                    .kernel
                                    .process_map_or(None, schedule_on, |process| {
                                        process.add_mpu_region(
                                            slice.ptr(),
                                            slice.len(),
                                            slice.len(),
                                        )
                                    });
                                schedule_on_kernel_data
                                    .schedule_upcall(
                                        to_schedule,
                                        (called_from_id, slice.len(), slice.ptr() as usize),
                                    )
                                    .ok();
                            }
                            Err(_) => {
                                schedule_on_kernel_data
                                    .schedule_upcall(to_schedule, (called_from_id, 0, 0))
                                    .ok();
                            }
                        }
                    }
                })
            })
            .and_then(|x| x)
    }
}

impl<const NUM_PROCS: usize, const NUM_UPCALLS: usize> SyscallDriver
    for IPC<NUM_PROCS, NUM_UPCALLS>
{
    /// command is how notify() is implemented.
    /// Notifying an IPC service is done by setting client_or_svc to 0,
    /// and notifying an IPC client is done by setting client_or_svc to 1.
    /// In either case, the target_id is the same number as provided in a notify
    /// upcall or as returned by allow.
    ///
    /// Returns INVAL if the other process doesn't exist.

    /// Initiates a service discovery or notifies a client or service.
    ///
    /// ### `command_num`
    ///
    /// - `0`: Driver check, always returns Ok(())
    /// - `1`: Perform discovery on the package name passed to `allow_readonly`. Returns the
    ///        service descriptor if the service is found, otherwise returns an error.
    /// - `2`: Notify a service previously discovered to have the service descriptor in
    ///        `target_id`. Returns an error if `target_id` refers to an invalid service or the
    ///        notify fails to enqueue.
    /// - `3`: Notify a client with descriptor `target_id`, typically in response to a previous
    ///        notify from the client. Returns an error if `target_id` refers to an invalid client
    ///        or the notify fails to enqueue.
    fn command(
        &self,
        command_number: usize,
        target_id: usize,
        _: usize,
        appid: ProcessId,
    ) -> CommandReturn {
        match command_number {
            0 => CommandReturn::success(),
            1 =>
            /* Discover */
            {
                self.data
                    .enter(appid, |_, kernel_data| {
                        kernel_data
                            .get_readonly_processbuffer(ro_allow::SEARCH)
                            .and_then(|search| {
                                search.enter(|slice| {
                                    self.data
                                        .kernel
                                        .process_until(|p| {
                                            let s = p.get_process_name().as_bytes();
                                            // are slices equal?
                                            if s.len() == slice.len()
                                                && s.iter()
                                                    .zip(slice.iter())
                                                    .all(|(c1, c2)| *c1 == c2.get())
                                            {
                                                // Return the index of the process which is used for
                                                // subscribe number
                                                p.processid()
                                                    .index()
                                                    .map(|i| CommandReturn::success_u32(i as u32))
                                            } else {
                                                None
                                            }
                                        })
                                        .unwrap_or(CommandReturn::failure(ErrorCode::NODEVICE))
                                })
                            })
                            .unwrap_or(CommandReturn::failure(ErrorCode::INVAL))
                    })
                    .unwrap_or(CommandReturn::failure(ErrorCode::NOMEM))
            }
            2 =>
            /* Service notify */
            {
                let cb_type = IPCUpcallType::Service;

                let other_process =
                    self.data
                        .kernel
                        .process_until(|p| match p.processid().index() {
                            Some(i) if i == target_id => Some(p.processid()),
                            _ => None,
                        });

                other_process.map_or(CommandReturn::failure(ErrorCode::INVAL), |otherapp| {
                    self.data.kernel.process_map_or(
                        CommandReturn::failure(ErrorCode::INVAL),
                        otherapp,
                        |target| {
                            let ret = target.enqueue_task(process::Task::IPC((appid, cb_type)));
                            match ret {
                                Ok(()) => CommandReturn::success(),
                                Err(e) => match e {
                                    // The other side has a null upcall, choosing to ignore
                                    ErrorCode::OFF => CommandReturn::success(),
                                    _ => CommandReturn::failure(e),
                                },
                            }
                        },
                    )
                })
            }
            3 =>
            /* Client notify */
            {
                let cb_type = IPCUpcallType::Client;

                let other_process =
                    self.data
                        .kernel
                        .process_until(|p| match p.processid().index() {
                            Some(i) if i == target_id => Some(p.processid()),
                            _ => None,
                        });

                other_process.map_or(CommandReturn::failure(ErrorCode::INVAL), |otherapp| {
                    self.data.kernel.process_map_or(
                        CommandReturn::failure(ErrorCode::INVAL),
                        otherapp,
                        |target| {
                            let ret = target.enqueue_task(process::Task::IPC((appid, cb_type)));
                            match ret {
                                Ok(()) => CommandReturn::success(),
                                Err(e) => match e {
                                    // The other side has a null upcall, choosing to ignore
                                    ErrorCode::OFF => CommandReturn::success(),
                                    _ => CommandReturn::failure(e),
                                },
                            }
                        },
                    )
                })
            }
            _ => CommandReturn::failure(ErrorCode::NOSUPPORT),
        }
    }

    fn allocate_grant(&self, processid: ProcessId) -> Result<(), crate::process::Error> {
        self.data.enter(processid, |_, _| {})
    }
}
