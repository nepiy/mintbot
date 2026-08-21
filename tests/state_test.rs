use nft_mint_bot::state::{AtomicBotState, BotState};
use std::{sync::Arc, thread};

#[test]
fn a_block_and_event_race_still_submits_once() {
    let state = Arc::new(AtomicBotState::new(BotState::WaitingForTrigger));
    let first = Arc::clone(&state);
    let second = Arc::clone(&state);
    let block = thread::spawn(move || first.try_acquire_trigger());
    let event = thread::spawn(move || second.try_acquire_trigger());

    let winners = [block.join().unwrap(), event.join().unwrap()]
        .into_iter()
        .filter(|won| *won)
        .count();
    assert_eq!(winners, 1);
    assert_eq!(state.load(), BotState::Triggered);
}
