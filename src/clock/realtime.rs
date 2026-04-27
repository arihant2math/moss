use crate::{
    drivers::timer::{Instant, now, uptime},
    sync::{OnceLock, SpinLock},
};
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::{task::Waker, time::Duration};
use libkernel::sync::waker_set::WakerSet;

#[derive(Default)]
struct RealtimeClockState {
    epoch: Option<(Duration, Instant)>,
    discontinuity_seq: u64,
    next_listener_id: u64,
    waiters: WakerSet,
    listeners: BTreeMap<u64, Arc<dyn Fn(u64) + Send + Sync>>,
}

fn clock_state() -> &'static SpinLock<RealtimeClockState> {
    static REALTIME_CLOCK: OnceLock<SpinLock<RealtimeClockState>> = OnceLock::new();
    REALTIME_CLOCK.get_or_init(|| SpinLock::new(RealtimeClockState::default()))
}

// Return a duration from the epoch.
pub fn date() -> Duration {
    let epoch_info = { clock_state().lock_save_irq().epoch };

    if let Some(ep_info) = epoch_info
        && let Some(now) = now()
    {
        let duration_since_ep_info = now - ep_info.1;
        ep_info.0 + duration_since_ep_info
    } else {
        uptime()
    }
}

pub fn set_date(duration: Duration) {
    if let Some(now) = now() {
        let (seq, callbacks) = {
            let mut state = clock_state().lock_save_irq();
            state.epoch = Some((duration, now));
            state.discontinuity_seq = state.discontinuity_seq.wrapping_add(1);
            state.waiters.wake_all();
            let seq = state.discontinuity_seq;
            let callbacks = state.listeners.values().cloned().collect::<Vec<_>>();
            (seq, callbacks)
        };

        for callback in callbacks {
            callback(seq);
        }
    }
}

pub fn discontinuity_seq() -> u64 {
    clock_state().lock_save_irq().discontinuity_seq
}

#[expect(dead_code)]
pub fn register_discontinuity_waker(waker: &Waker) -> u64 {
    clock_state().lock_save_irq().waiters.register(waker)
}

#[expect(dead_code)]
pub fn remove_discontinuity_waker(token: u64) {
    clock_state().lock_save_irq().waiters.remove(token);
}

pub fn register_change_listener(callback: Arc<dyn Fn(u64) + Send + Sync>) -> u64 {
    let mut state = clock_state().lock_save_irq();
    let id = state.next_listener_id;
    state.next_listener_id = state.next_listener_id.wrapping_add(1);
    state.listeners.insert(id, callback);
    id
}

pub fn unregister_change_listener(id: u64) {
    clock_state().lock_save_irq().listeners.remove(&id);
}

pub fn monotonic_deadline_for(target: Duration) -> Option<Instant> {
    let remaining = target.saturating_sub(date());
    now().map(|now| now + remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    use moss_macros::ktest;

    #[ktest]
    fn test_date_and_set_date() {
        let initial_date = date();
        let new_date = Duration::from_secs(1_000_000);
        set_date(new_date);
        let updated_date = date();
        assert_ne!(
            initial_date, updated_date,
            "Date should change after set_date"
        );
        assert!(
            updated_date >= new_date,
            "Updated date should be at least the new date set"
        );
    }
}
