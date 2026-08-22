// SPDX-License-Identifier: GPL-3.0-or-later
//
// The lock screen this compositor draws itself, and the PAM conversation
// behind it.
//
// Two halves live here and they answer two different questions.
//
// [`Mode`] answers "what does locking mean on this machine". It used to be one
// answer — run `idle.lock_command` — and the whole of the compositor's part
// was being a correct ext-session-lock *server* for whatever that program
// turned out to be. That is still the answer when the key is set, because
// somebody who put `swaylock -f` in their config asked for swaylock and should
// keep getting it. With no key set the answer is now the shell's own lock
// screen rather than nothing at all, which is the change: a `lock` binding
// that logged "nothing to run" was a lock screen that did not exist on the
// default configuration, and a touch-only desk could lock — the lid, the idle
// deadline — and then had no way back in, because swaylock has no keyboard of
// its own and cannot reach `data/shell/osk.js`.
//
// [`Authenticator`] answers "is this the right password", and it answers it on
// a thread. That is not an optimisation. `pam_authenticate` reads a file,
// hashes with a deliberately slow KDF, and on a wrong password sleeps for up
// to two seconds inside `pam_fail_delay` before it returns; pam_sss or
// pam_krb5 talk to the network and can take much longer than that. Every one
// of those is a stall of the whole desk if it happens on the event loop —
// nothing renders, no input is read, and the lock screen the person is typing
// into stops repainting the moment they press Enter. So the attempt is posted
// to a worker and the verdict comes back through a calloop channel, the same
// arrangement the launcher's scanner and the status sampler use.
//
// The worker is one thread and takes one attempt at a time. That is also the
// rate limit: PAM's own failure delay is serialised behind it, so a script
// firing passwords at the control socket gets them checked at whatever rate
// PAM is willing to check them and not one faster.
//
// libpam is opened with `dlopen` rather than linked. Three reasons, in the
// order they matter: the compositor must still build and run on a machine
// without PAM development files, because it is the thing that draws the
// screen and refusing to start over an authentication library leaves nothing
// to log in to and fix it with; a missing libpam then fails in the safe
// direction, which is a lock screen that refuses every password rather than
// one that accepts any; and it costs no new crate in `Cargo.lock`, which
// matters here because the alternative crates all wrap the same six functions
// this file names.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::mpsc;

use viewport_ipc::request::Secret;

/// What locking means on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Run somebody else's locker. `idle.lock_command`, verbatim.
    Command(String),
    /// Draw the lock screen in the shell.
    BuiltIn,
}

impl Mode {
    /// The one answer, from the one setting.
    ///
    /// Every caller of `lock_session` goes through this — the idle deadline,
    /// the `lock` binding, the lid, the power menu's Lock row — so there is
    /// one place that decides and no way for two of them to decide
    /// differently. That was already the property `lock_session` was
    /// documented to have; this keeps it while there are two things it could
    /// mean.
    ///
    /// An empty or whitespace-only `lock_command` is the built-in screen, not
    /// a command. `"lock_command": ""` reads as "no locker" to a person
    /// writing the file, and spawning it would be a shell running the empty
    /// string once a lock.
    pub fn from_command(command: Option<&str>) -> Self {
        match command.map(str::trim).filter(|c| !c.is_empty()) {
            Some(command) => Self::Command(command.to_owned()),
            None => Self::BuiltIn,
        }
    }

    /// Whether this is the shell's own lock screen.
    pub fn is_built_in(&self) -> bool {
        matches!(self, Self::BuiltIn)
    }
}

/// One password, and which lock it was typed at.
pub struct Attempt {
    pub generation: u64,
    pub password: Secret,
}

/// What PAM said.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub generation: u64,
    pub ok: bool,
    /// PAM's own words, for a refusal that is not simply a wrong password —
    /// an expired account, a locked one, a module that could not be reached.
    pub message: Option<String>,
}

/// The half of the authenticator the event loop talks to.
#[derive(Default)]
pub struct Authenticator {
    /// The worker's mailbox, once there is a worker. Absent where the thread
    /// could not be started, which [`Authenticator::online`] reports and the
    /// lock screen says out loud.
    mailbox: Option<mpsc::Sender<Attempt>>,
}

impl Authenticator {
    /// Start the thread, delivering its verdicts through `sink`.
    ///
    /// Started once at boot rather than at the first lock, and deliberately:
    /// a thread that has to be spawned while the screen is being locked is a
    /// thread that can fail to spawn while the screen is being locked, and the
    /// person is by then looking at a password box. Starting it early makes
    /// [`online`](Self::online) a fact the lock screen can be told at the
    /// moment it is drawn.
    ///
    /// The thread costs a stack and nothing else until something is posted to
    /// it: libpam is not opened until the first attempt, so a session that is
    /// never locked never loads it.
    pub fn start(
        &mut self,
        sink: smithay::reexports::calloop::channel::Sender<Verdict>,
    ) -> std::io::Result<()> {
        let (sender, attempts) = mpsc::channel::<Attempt>();
        std::thread::Builder::new()
            .name("viewport-auth".to_owned())
            .spawn(move || {
                // Read once, at birth. Both are session constants and both
                // want the environment, which this thread should touch as
                // little as it can while the compositor's other threads are
                // live: see the note about `environ` in `apply_config`.
                let user = current_user();
                let service = service_name();
                let mut pam = None;
                for attempt in attempts {
                    let verdict = match user.as_deref() {
                        Some(user) => {
                            // Opened on the first attempt and kept: dlopen of
                            // a library already mapped is cheap, but the log
                            // line about it failing is not something to print
                            // once a keystroke.
                            let library = pam.get_or_insert_with(Library::open);
                            match library.as_ref() {
                                Ok(library) => {
                                    authenticate(library, &service, user, &attempt.password)
                                }
                                Err(e) => Err(e.clone()),
                            }
                        }
                        // No user to authenticate as. Refused rather than
                        // guessed at: `pam_start` with a wrong name asks the
                        // stack about somebody else's account.
                        None => Err("there is no user to authenticate".to_owned()),
                    };
                    let verdict = Verdict {
                        generation: attempt.generation,
                        ok: verdict.is_ok(),
                        message: verdict.err(),
                    };
                    if sink.send(verdict).is_err() {
                        // A closed channel is the compositor going away.
                        return;
                    }
                }
            })?;
        self.mailbox = Some(sender);
        Ok(())
    }

    /// Whether there is a worker to ask.
    ///
    /// False is a lock screen that cannot be unlocked from the front. It is
    /// still a lock screen — see the module comment on which direction to fail
    /// in — and the page is told so it can say so.
    pub fn online(&self) -> bool {
        self.mailbox.is_some()
    }

    /// Hand an attempt to the thread.
    ///
    /// Returns false when there is nobody to hand it to, so the caller can
    /// answer the page rather than leaving it waiting for a verdict that will
    /// never come.
    pub fn ask(&self, attempt: Attempt) -> bool {
        match self.mailbox.as_ref() {
            Some(mailbox) => mailbox.send(attempt).is_ok(),
            None => false,
        }
    }
}

/// Which PAM service to authenticate against.
///
/// `viewport` if the system has a policy for this compositor by name, and
/// `login` otherwise. The name decides which stack runs, so getting it wrong
/// is not a detail: a service with no file is handled by `/etc/pam.d/other`,
/// which on a correctly configured system denies everything — a lock screen
/// that never opens — and on a badly configured one permits everything, which
/// is worse. Falling back to `login`, which every distribution ships and which
/// authenticates a local user against the local password, avoids both.
///
/// `$VIEWPORT_PAM_SERVICE` overrides it, for the machine whose stack is
/// somewhere else entirely.
fn service_name() -> String {
    if let Some(asked) = std::env::var_os("VIEWPORT_PAM_SERVICE") {
        return asked.to_string_lossy().into_owned();
    }
    if std::path::Path::new("/etc/pam.d/viewport").exists() {
        return "viewport".to_owned();
    }
    "login".to_owned()
}

/// The name of the user this compositor is running as.
///
/// From the password database rather than from `$USER`, which is inherited and
/// can say anything. Authenticating the name in the environment rather than the
/// name behind the uid would let a session started with `USER=root viewport`
/// ask the stack about root's password — and, worse, let a correct root
/// password unlock somebody else's screen.
fn current_user() -> Option<String> {
    // SAFETY: `getpwuid_r` writes into the buffer it is given and reports the
    // overflow rather than running past it. The `passwd` it fills points into
    // that same buffer, which outlives the read below.
    unsafe {
        let uid = libc::getuid();
        let mut passwd: libc::passwd = std::mem::zeroed();
        let mut buffer = vec![0_i8; 4096];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let code = libc::getpwuid_r(
            uid,
            &mut passwd,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        );
        if code != 0 || result.is_null() || passwd.pw_name.is_null() {
            return None;
        }
        CStr::from_ptr(passwd.pw_name)
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

// The PAM constants this file uses. Named here rather than pulled from a
// binding crate, because they are ABI and have not moved in twenty years.
const PAM_SUCCESS: c_int = 0;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 19;
/// Refuse an empty password outright rather than letting a module decide that
/// a blank one is fine. A lock screen where Enter is the password is not one.
const PAM_DISALLOW_NULL_AUTHTOK: c_int = 0x0001;
/// Refresh the credentials rather than granting new ones: this session already
/// exists and is only being handed back to the person who left it. Kerberos
/// and friends want to hear about that; `PAM_ESTABLISH_CRED` would be a lie.
const PAM_REFRESH_CRED: c_int = 0x0010;

#[repr(C)]
struct PamMessage {
    style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    retcode: c_int,
}

#[repr(C)]
struct PamConv {
    conv: Option<
        unsafe extern "C" fn(
            c_int,
            *const *const PamMessage,
            *mut *mut PamResponse,
            *mut c_void,
        ) -> c_int,
    >,
    appdata: *mut c_void,
}

// The six signatures, from `<security/pam_appl.h>`. Named rather than written
// inline because each one is transmuted from a `dlsym` result, and a transmute
// to an inferred function type is a way to get the ABI wrong in silence.
type PamStart =
    unsafe extern "C" fn(*const c_char, *const c_char, *const PamConv, *mut *mut c_void) -> c_int;
type PamCall = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type PamStrerror = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char;

/// libpam, opened at runtime.
///
/// The handle is deliberately never closed. `dlclose` on a library whose
/// modules have registered atexit handlers and thread-local state is a way to
/// crash at exit, and this process opens it once and keeps it for its life.
struct Library {
    start: PamStart,
    authenticate: PamCall,
    acct_mgmt: PamCall,
    setcred: PamCall,
    end: PamCall,
    strerror: PamStrerror,
}

// SAFETY: function pointers into a library that is never unloaded. Nothing
// here is a handle to per-thread state; the `pam_handle_t` that is one never
// leaves the thread that made it.
unsafe impl Send for Library {}

impl Library {
    fn open() -> Result<Self, String> {
        // SAFETY: a plain dlopen/dlsym of a library by soname. Every symbol is
        // checked for null before it is transmuted, and the signatures below
        // are `<security/pam_appl.h>`'s.
        unsafe {
            let handle = libc::dlopen(
                c"libpam.so.0".as_ptr(),
                // Global, because a PAM module loaded by the stack resolves
                // `pam_get_item` and friends against whatever is already in
                // the process. Loaded locally, a module that does not link
                // libpam itself fails to resolve and the whole stack errors.
                libc::RTLD_NOW | libc::RTLD_GLOBAL,
            );
            if handle.is_null() {
                let reason = CStr::from_ptr(libc::dlerror())
                    .to_string_lossy()
                    .into_owned();
                return Err(format!(
                    "libpam.so.0 could not be loaded, so no password can be checked: {reason}"
                ));
            }
            let symbol = |name: &CStr| -> Result<*mut c_void, String> {
                let found = libc::dlsym(handle, name.as_ptr());
                if found.is_null() {
                    return Err(format!(
                        "libpam.so.0 has no {}, so no password can be checked",
                        name.to_string_lossy()
                    ));
                }
                Ok(found)
            };
            Ok(Self {
                start: std::mem::transmute::<*mut c_void, PamStart>(symbol(c"pam_start")?),
                authenticate: std::mem::transmute::<*mut c_void, PamCall>(symbol(
                    c"pam_authenticate",
                )?),
                acct_mgmt: std::mem::transmute::<*mut c_void, PamCall>(symbol(c"pam_acct_mgmt")?),
                setcred: std::mem::transmute::<*mut c_void, PamCall>(symbol(c"pam_setcred")?),
                end: std::mem::transmute::<*mut c_void, PamCall>(symbol(c"pam_end")?),
                strerror: std::mem::transmute::<*mut c_void, PamStrerror>(symbol(c"pam_strerror")?),
            })
        }
    }
}

/// The conversation function PAM calls to ask for the password.
///
/// `appdata` is a `*const CString` owned by the frame below, which outlives
/// every call: PAM only calls this from inside `pam_authenticate`.
///
/// Every prompt gets the same answer, which is what a lock screen can offer —
/// there is one box and it was filled in before PAM was asked anything. A
/// stack that asks two different questions (a password and then a one-time
/// code) therefore cannot be satisfied here, and fails rather than being
/// half-answered.
///
/// # Safety
///
/// Called by libpam with the ABI in `<security/pam_appl.h>`.
unsafe extern "C" fn converse(
    count: c_int,
    messages: *const *const PamMessage,
    responses: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if count <= 0 || messages.is_null() || responses.is_null() || appdata.is_null() {
        return PAM_CONV_ERR;
    }
    // SAFETY: the contract above.
    unsafe {
        let password = &*(appdata as *const CString);

        // calloc rather than a Rust allocation: PAM frees this array with
        // `free`, and every string in it with `free`, whatever happens next.
        // Handing it something Rust allocated is a heap corruption that shows
        // up somewhere else entirely.
        let array =
            libc::calloc(count as usize, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
        if array.is_null() {
            return PAM_BUF_ERR;
        }

        for at in 0..count as isize {
            let message = *messages.offset(at);
            if message.is_null() {
                continue;
            }
            let slot = array.offset(at);
            match (*message).style {
                PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON => {
                    let copy = libc::strdup(password.as_ptr());
                    if copy.is_null() {
                        libc::free(array.cast());
                        return PAM_BUF_ERR;
                    }
                    (*slot).resp = copy;
                }
                // An error or a notice. PAM wants the slot left empty; the
                // text is picked up from `pam_strerror` on the way out if the
                // attempt fails, so nothing is lost by not reading it here.
                _ => (*slot).resp = std::ptr::null_mut(),
            }
            (*slot).retcode = 0;
        }
        *responses = array;
        PAM_SUCCESS
    }
}

/// One whole PAM conversation, start to end.
///
/// Three calls rather than one, and the second is the one people leave out:
///
/// * `pam_authenticate` — is this the password.
/// * `pam_acct_mgmt` — *and is this account still allowed to log in*. An
///   expired password, a disabled account or a time restriction all pass
///   authentication and fail here, and a lock screen that skips it unlocks a
///   session the administrator has already ended.
/// * `pam_setcred(PAM_REFRESH_CRED)` — renew whatever ticket the stack keeps.
///   A failure is logged and not fatal: the password was right, and refusing
///   to unlock over an unrenewable Kerberos ticket would leave somebody locked
///   out of their own running session.
///
/// `Ok` is "unlock the session". `Err` carries the sentence to put on the lock
/// screen, which is PAM's own wherever there is one.
fn authenticate(
    library: &Library,
    service: &str,
    user: &str,
    password: &Secret,
) -> Result<(), String> {
    let (Ok(service_c), Ok(user_c), Ok(password_c)) = (
        CString::new(service),
        CString::new(user),
        CString::new(password.expose()),
    ) else {
        // An interior NUL. In the password it is somebody pasting a binary
        // file into the box; there is no way to hand it to a C API, and no
        // password with a NUL in it was ever set by `passwd` either.
        return Err("that is not a password this machine could have".to_owned());
    };

    let conversation = PamConv {
        conv: Some(converse),
        appdata: (&password_c as *const CString) as *mut c_void,
    };
    let mut handle: *mut c_void = std::ptr::null_mut();

    // SAFETY: the four calls below are the documented sequence, on a handle
    // this function owns from `pam_start` to `pam_end` and never shares. The
    // conversation's `appdata` points at `password_c`, which lives until the
    // end of this frame — past `pam_end`, which is the last call that could
    // reach it.
    unsafe {
        let code = (library.start)(
            service_c.as_ptr(),
            user_c.as_ptr(),
            &conversation,
            &mut handle,
        );
        if code != PAM_SUCCESS || handle.is_null() {
            // `pam_strerror` needs a handle and there may not be one, so this
            // is the one message that is ours rather than PAM's.
            return Err(format!("the {service} PAM stack could not be started"));
        }

        let say = |code: c_int| -> String {
            let text = (library.strerror)(handle, code);
            if text.is_null() {
                return format!("PAM returned {code}");
            }
            CStr::from_ptr(text).to_string_lossy().into_owned()
        };

        let code = (library.authenticate)(handle, PAM_DISALLOW_NULL_AUTHTOK);
        if code != PAM_SUCCESS {
            let message = say(code);
            (library.end)(handle, code);
            return Err(message);
        }

        let code = (library.acct_mgmt)(handle, PAM_DISALLOW_NULL_AUTHTOK);
        if code != PAM_SUCCESS {
            let message = say(code);
            (library.end)(handle, code);
            return Err(message);
        }

        let code = (library.setcred)(handle, PAM_REFRESH_CRED);
        if code != PAM_SUCCESS {
            tracing::warn!(
                "lock: the password was right but the credentials could not be \
                 refreshed ({}). Unlocking anyway — a session its own user \
                 cannot get back into is the worse failure.",
                say(code)
            );
        }

        (library.end)(handle, PAM_SUCCESS);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one setting, and what each shape of it means.
    #[test]
    fn an_absent_lock_command_is_the_built_in_screen() {
        assert_eq!(Mode::from_command(None), Mode::BuiltIn);
        assert_eq!(
            Mode::from_command(Some("swaylock -f")),
            Mode::Command("swaylock -f".to_owned())
        );
    }

    /// `"lock_command": ""` is not a command to run.
    ///
    /// Spawning it would be a shell started once per lock to run nothing,
    /// while the screen showed the desktop — a session that reports itself
    /// locked and is not.
    #[test]
    fn an_empty_lock_command_is_not_a_locker() {
        assert_eq!(Mode::from_command(Some("")), Mode::BuiltIn);
        assert_eq!(Mode::from_command(Some("   ")), Mode::BuiltIn);
    }

    /// A password must not be printable by accident.
    #[test]
    fn a_password_does_not_print_itself() {
        let secret = Secret("hunter2".to_owned());
        assert_eq!(format!("{secret:?}"), "<secret>");
        assert!(!format!("{secret:?}").contains("hunter2"));
        assert_eq!(secret.expose(), "hunter2");
    }

    /// And not through the request that carries it either, which is the shape
    /// `apply`'s refusal lines actually print.
    #[test]
    fn a_refused_unlock_does_not_print_the_password() {
        let request = viewport_ipc::Request::SessionUnlock {
            generation: 7,
            password: Secret("hunter2".to_owned()),
        };
        let printed = format!("{request:?}");
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("<secret>"), "{printed}");
    }
}
