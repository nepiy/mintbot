use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BotState {
    Starting = 0,
    LoadingConfiguration = 1,
    ConnectingRpc = 2,
    Validating = 3,
    Preparing = 4,
    Armed = 5,
    WaitingForTrigger = 6,
    Triggered = 7,
    Signing = 8,
    Broadcasting = 9,
    Submitted = 10,
    Confirmed = 11,
    Failed = 12,
    Stopped = 13,
}

impl TryFrom<u8> for BotState {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Starting),
            1 => Ok(Self::LoadingConfiguration),
            2 => Ok(Self::ConnectingRpc),
            3 => Ok(Self::Validating),
            4 => Ok(Self::Preparing),
            5 => Ok(Self::Armed),
            6 => Ok(Self::WaitingForTrigger),
            7 => Ok(Self::Triggered),
            8 => Ok(Self::Signing),
            9 => Ok(Self::Broadcasting),
            10 => Ok(Self::Submitted),
            11 => Ok(Self::Confirmed),
            12 => Ok(Self::Failed),
            13 => Ok(Self::Stopped),
            other => Err(other),
        }
    }
}

pub struct AtomicBotState(AtomicU8);

impl Default for AtomicBotState {
    fn default() -> Self {
        Self::new(BotState::Starting)
    }
}

impl AtomicBotState {
    pub const fn new(state: BotState) -> Self {
        Self(AtomicU8::new(state as u8))
    }

    pub fn load(&self) -> BotState {
        BotState::try_from(self.0.load(Ordering::Acquire)).unwrap_or(BotState::Failed)
    }

    pub fn store(&self, state: BotState) {
        self.0.store(state as u8, Ordering::Release);
    }

    pub fn try_acquire_trigger(&self) -> bool {
        self.0
            .compare_exchange(
                BotState::WaitingForTrigger as u8,
                BotState::Triggered as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn only_one_trigger_wins() {
        let state = Arc::new(AtomicBotState::new(BotState::WaitingForTrigger));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let state = Arc::clone(&state);
            handles.push(thread::spawn(move || state.try_acquire_trigger()));
        }
        let winners = handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
        assert_eq!(state.load(), BotState::Triggered);
    }
}
