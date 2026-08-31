use std::io;

#[cfg(unix)]
use std::mem::MaybeUninit;
#[cfg(unix)]
use std::sync::Mutex;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
static PASSWORD_PROMPT_LOCK: Mutex<()> = Mutex::new(());
#[cfg(unix)]
static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Read a password while ensuring Ctrl-C cannot strand the terminal in raw mode.
pub fn read_password() -> io::Result<String> {
    #[cfg(unix)]
    {
        read_password_unix()
    }

    #[cfg(not(unix))]
    {
        read_password_with_stderr_output()
    }
}

fn read_password_with_stderr_output() -> io::Result<String> {
    let config = rpassword::ConfigBuilder::new()
        .output_writer(io::stderr())
        .build();
    rpassword::read_password_with_config(config)
}

#[cfg(unix)]
fn read_password_unix() -> io::Result<String> {
    let _lock = PASSWORD_PROMPT_LOCK
        .lock()
        .map_err(|_| io::Error::other("password prompt lock was poisoned"))?;
    SIGINT_RECEIVED.store(false, Ordering::SeqCst);

    let mut deferred_sigint = DeferredSigint::install().map_err(|error| {
        io::Error::new(error.kind(), format!("failed to defer SIGINT: {error}"))
    })?;
    let result = read_password_with_stderr_output();
    let mut interrupted = SIGINT_RECEIVED.swap(false, Ordering::SeqCst);
    deferred_sigint.restore().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to restore the SIGINT handler: {error}"),
        )
    })?;
    interrupted |= SIGINT_RECEIVED.swap(false, Ordering::SeqCst);

    if interrupted {
        // rpassword has dropped its raw-mode guard by this point, so re-delivering
        // SIGINT preserves normal Ctrl-C semantics without leaving echo disabled.
        if unsafe { libc::raise(libc::SIGINT) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
    }

    result
}

#[cfg(unix)]
unsafe extern "C" fn defer_sigint(_signal: libc::c_int) {
    SIGINT_RECEIVED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
struct DeferredSigint {
    previous: libc::sigaction,
    restored: bool,
}

#[cfg(unix)]
impl DeferredSigint {
    fn install() -> io::Result<Self> {
        let mut action = MaybeUninit::<libc::sigaction>::zeroed();
        let action = unsafe {
            let action = action.assume_init_mut();
            action.sa_sigaction = defer_sigint as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = 0;
            action
        };
        let mut previous = MaybeUninit::<libc::sigaction>::uninit();
        if unsafe { libc::sigaction(libc::SIGINT, action, previous.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            previous: unsafe { previous.assume_init() },
            restored: false,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        if unsafe { libc::sigaction(libc::SIGINT, &self.previous, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.restored = true;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for DeferredSigint {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
