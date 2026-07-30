// SPDX-License-Identifier: GPL-3.0-or-later
//
// What the frame clock has to be able to do.
//
// Under the web engine the compositor does not do its own waiting: GLib owns
// the blocking poll and watches calloop's epoll fd as one source. So a timer
// that is going to invite clients to paint on an otherwise still desktop has
// to be visible to *that* poll, not only to calloop's own bookkeeping.
//
// These two tests are the same experiment run against the two kinds of timer,
// and the difference between them is the bug: a terminal that showed nothing
// until the mouse moved.

use std::time::Duration;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::rustix::event::{poll, PollFd, PollFlags};
use smithay::reexports::rustix::time::{
    timerfd_create, timerfd_settime, Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags,
    Timespec,
};

use std::os::fd::AsFd;

/// How long to wait for a timer set for 20ms. Generous: this is asking whether
/// the wakeup happens at all, not when.
const PATIENCE: Timespec = Timespec {
    tv_sec: 1,
    tv_nsec: 0,
};

fn in_twenty_ms() -> Itimerspec {
    Itimerspec {
        it_interval: Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: Timespec {
            tv_sec: 0,
            tv_nsec: 20_000_000,
        },
    }
}

/// A timerfd wakes a poll on the loop's fd, which is what the frame clock needs.
#[test]
fn a_timerfd_wakes_an_outer_poll() {
    let mut event_loop: EventLoop<'static, bool> = EventLoop::try_new().unwrap();

    let timer = timerfd_create(
        TimerfdClockId::Monotonic,
        TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC,
    )
    .unwrap();
    let watched = timer.try_clone().unwrap();

    event_loop
        .handle()
        .insert_source(
            Generic::new(watched, Interest::READ, Mode::Level),
            |_, fd, ticked: &mut bool| {
                let mut buf = [0u8; 8];
                let _ = smithay::reexports::rustix::io::read(&*fd, &mut buf[..]);
                *ticked = true;
                Ok(PostAction::Continue)
            },
        )
        .unwrap();

    timerfd_settime(&timer, TimerfdTimerFlags::empty(), &in_twenty_ms()).unwrap();

    // Standing in for GLib: block on the loop's fd and nothing else.
    let loop_fd = event_loop.as_fd();
    let mut fds = [PollFd::new(&loop_fd, PollFlags::IN)];
    let woke = poll(&mut fds, Some(&PATIENCE)).unwrap();
    assert_eq!(
        woke, 1,
        "the timer expired and the loop's fd stayed quiet, so an outer loop \
         would still be asleep"
    );

    let mut ticked = false;
    event_loop
        .dispatch(Some(Duration::ZERO), &mut ticked)
        .unwrap();
    assert!(ticked, "the loop woke but the tick did not run");
}

/// And calloop's own timer does not, which is why the clock is not one.
///
/// Not a complaint about calloop: its timers live in a wheel it consults while
/// it is the one waiting, and here it is not. The test is here so that anyone
/// moving the frame clock back onto `Timer` finds out immediately rather than
/// from a frozen terminal.
#[test]
fn a_calloop_timer_does_not() {
    let event_loop: EventLoop<'static, bool> = EventLoop::try_new().unwrap();

    event_loop
        .handle()
        .insert_source(
            smithay::reexports::calloop::timer::Timer::from_duration(Duration::from_millis(20)),
            |_, _, ticked: &mut bool| {
                *ticked = true;
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            },
        )
        .unwrap();

    let loop_fd = event_loop.as_fd();
    let mut fds = [PollFd::new(&loop_fd, PollFlags::IN)];
    let woke = poll(
        &mut fds,
        Some(&Timespec {
            tv_sec: 0,
            tv_nsec: 200_000_000,
        }),
    )
    .unwrap();
    assert_eq!(
        woke, 0,
        "a calloop timer became visible to an outer poll — if that is now true \
         the frame clock could go back to using one"
    );
}
