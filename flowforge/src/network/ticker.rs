use std::fmt::Debug;

use crate::{
    core::never::Never,
    quantities::{Time, TimeSpan},
    simulation::{Component, EffectContext, Message},
};

pub struct Ticker<F>
where
    F: Fn(),
{
    next_tick: Time,
    interval: TimeSpan,
    action: F,
}

impl<F> Debug for Ticker<F>
where
    F: Fn(),
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ticker")
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

impl<F> Ticker<F>
where
    F: Fn(),
{
    pub const fn new(interval: TimeSpan, action: F) -> Ticker<F> {
        Ticker {
            next_tick: Time::SIM_START,
            interval,
            action,
        }
    }
}

impl<'sim, E, F> Component<'sim, E> for Ticker<F>
where
    F: Fn(),
{
    type Receive = Never;

    fn next_tick(&self, _time: Time) -> Option<Time> {
        Some(self.next_tick)
    }

    fn tick(&mut self, _context: EffectContext) -> Vec<Message<'sim, E>> {
        (self.action)();
        vec![]
    }

    fn receive(&mut self, _e: Self::Receive, _context: EffectContext) -> Vec<Message<'sim, E>> {
        panic!()
    }
}
