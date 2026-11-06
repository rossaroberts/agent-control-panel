//! The due timer's one wait: until a given time, or until poked. On macOS a kqueue `EVFILT_TIMER` with `NOTE_CRITICAL`
//! (a plain timed wait, condvar or sleep, is coalesced by the system and comes up to ~150 ms late on a quiet Mac),
//! woken early by an `EVFILT_USER`. Elsewhere a condvar.

use std::io;
use std::time::Duration;

#[cfg(target_os = "macos")]
pub struct Alarm {
    kq: std::ffi::c_int,
}

#[cfg(target_os = "macos")]
const POKE: usize = 0;
#[cfg(target_os = "macos")]
const TIMER: usize = 1;

#[cfg(target_os = "macos")]
fn ev(ident: usize, filter: i16, flags: u16, fflags: u32, data: isize) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data,
        udata: std::ptr::null_mut(),
    }
}

#[cfg(target_os = "macos")]
impl Alarm {
    pub fn new() -> io::Result<Alarm> {
        // SAFETY: no arguments; the fd is ours to close.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        let a = Alarm { kq };
        a.submit(ev(
            POKE,
            libc::EVFILT_USER,
            libc::EV_ADD | libc::EV_CLEAR,
            0,
            0,
        ))?;
        Ok(a)
    }

    fn submit(&self, e: libc::kevent) -> io::Result<()> {
        // SAFETY: one change, no event list.
        let r = unsafe { libc::kevent(self.kq, &e, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Makes a `wait` under way (or the next one) return.
    pub fn poke(&self) {
        let _ = self.submit(ev(POKE, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0));
    }

    /// Blocks until poked, or until `after` has passed (`None`: until poked).
    pub fn wait(&self, after: Option<Duration>) {
        let armed = match after {
            Some(d) => {
                let ms = d.as_millis().clamp(1, isize::MAX as u128) as isize;
                self.submit(ev(
                    TIMER,
                    libc::EVFILT_TIMER,
                    libc::EV_ADD | libc::EV_ONESHOT,
                    libc::NOTE_CRITICAL,
                    ms,
                ))
                .is_ok()
            }
            None => {
                // An earlier timer that has not fired is not wanted any more (an error: there was none).
                let _ = self.submit(ev(TIMER, libc::EVFILT_TIMER, libc::EV_DELETE, 0, 0));
                true
            }
        };
        let mut out = ev(0, 0, 0, 0, 0);
        // With no timer (it could not be armed), never sleep past the time asked for.
        let ts = after.filter(|_| !armed).map(|d| libc::timespec {
            tv_sec: d.as_secs() as libc::time_t,
            tv_nsec: d.subsec_nanos() as libc::c_long,
        });
        let tp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const _);
        // SAFETY: room for one event; no changes.
        unsafe { libc::kevent(self.kq, std::ptr::null(), 0, &mut out, 1, tp) };
    }
}

#[cfg(target_os = "macos")]
impl Drop for Alarm {
    fn drop(&mut self) {
        // SAFETY: ours.
        unsafe { libc::close(self.kq) };
    }
}

#[cfg(not(target_os = "macos"))]
pub struct Alarm {
    poked: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

#[cfg(not(target_os = "macos"))]
impl Alarm {
    pub fn new() -> io::Result<Alarm> {
        Ok(Alarm {
            poked: std::sync::Mutex::new(false),
            cv: std::sync::Condvar::new(),
        })
    }

    pub fn poke(&self) {
        *self.poked.lock().unwrap() = true;
        self.cv.notify_one();
    }

    pub fn wait(&self, after: Option<Duration>) {
        let g = self.poked.lock().unwrap();
        let mut g = match after {
            Some(d) => self.cv.wait_timeout_while(g, d, |p| !*p).unwrap().0,
            None => self.cv.wait_while(g, |p| !*p).unwrap(),
        };
        *g = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn it_wakes_on_time_and_when_poked() {
        let a = Alarm::new().unwrap();
        for ms in [50u64, 300] {
            let t = Instant::now();
            a.wait(Some(Duration::from_millis(ms)));
            let late = t.elapsed().as_secs_f64() * 1000.0 - ms as f64;
            assert!((-1.0..20.0).contains(&late), "{ms} ms: {late:.2} ms late");
        }
        a.poke();
        let t = Instant::now();
        a.wait(None);
        assert!(
            t.elapsed() < Duration::from_millis(50),
            "a poke before the wait counts"
        );
        // A poke ends a long wait at once.
        let a = std::sync::Arc::new(a);
        let b = a.clone();
        let p = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            b.poke();
        });
        let t = Instant::now();
        a.wait(Some(Duration::from_secs(10)));
        assert!(t.elapsed() < Duration::from_secs(1));
        p.join().unwrap();
    }
}
