use core::future::Future as _;
use core::pin::Pin;
use core::task::{ready, Context, Poll};

use crate::async_vm_try;
use crate::runtime::{Future, Output, Select, Vm, VmError};

/// A stored await task.
#[derive(Debug)]
pub(crate) enum Awaited {
    /// A future to be awaited.
    Future(Future, Output),
    /// A select to be awaited.
    Select(Select, Output),
    /// An operation for the host, named by the low 32 bits of a marker hash.
    HostOp(u64, Output),
}

/// The tag in the high 32 bits of an `Inline::Hash` which marks an operation
/// for the host.
///
/// Awaiting such a value halts the virtual machine with the operation instead
/// of polling a future. Only [`VmExecution::resume_host`] runs such an
/// execution: the host stores the result of the operation and resumes it.
///
/// [`VmExecution::resume_host`]: crate::runtime::VmExecution::resume_host
pub const HOST_OP_TAG: u64 = 0x484F_5354;

impl Awaited {
    pub(crate) fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        vm: &mut Vm,
    ) -> Poll<Result<(), VmError>> {
        let this = unsafe { Pin::get_unchecked_mut(self) };

        match *this {
            Self::Future(ref mut future, out) => {
                let future = unsafe { Pin::new_unchecked(future) };
                let result = ready!(future.poll(cx));
                let value = async_vm_try!(VmError::with_vm(result, vm));
                async_vm_try!(vm.store(out, value));
            }
            Self::HostOp(..) => {
                return Poll::Ready(Err(VmError::new(crate::runtime::VmErrorKind::Halted {
                    halt: crate::runtime::VmHaltInfo::Awaited,
                })));
            }
            Self::Select(ref mut select, out) => {
                let select = unsafe { Pin::new_unchecked(select) };
                let result = ready!(select.poll(cx));
                let (ip, value) = async_vm_try!(VmError::with_vm(result, vm));
                vm.set_ip(ip);
                async_vm_try!(vm.store(out, || value));
            }
        }

        Poll::Ready(Ok(()))
    }
}
