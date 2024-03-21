use derive_where::derive_where;

use crate::{
    never::Never,
    quantities::{Time, TimeSpan},
    rand::{PositiveContinuousDistribution, Rng},
    simulation::{Component, EffectContext, Message, MessageDestination},
};

#[derive(PartialEq, Eq, Debug)]
pub enum Toggle {
    Enable,
    Disable,
}

#[derive_where(Debug)]
pub struct Toggler<'sim, E> {
    target: MessageDestination<'sim, Toggle, E>,
    enabled: bool,
    on_distribution: PositiveContinuousDistribution<TimeSpan>,
    off_distribution: PositiveContinuousDistribution<TimeSpan>,
    next_toggle: Time,
    rng: Rng,
}

impl<'sim, E> Toggler<'sim, E> {
    #[must_use]
    pub fn new(
        target: MessageDestination<'sim, Toggle, E>,
        on_distribution: PositiveContinuousDistribution<TimeSpan>,
        off_distribution: PositiveContinuousDistribution<TimeSpan>,
        mut rng: Rng,
    ) -> Toggler<'sim, E> {
        Toggler {
            target,
            enabled: false,
            next_toggle: Time::from_sim_start(rng.sample(&off_distribution)),
            on_distribution,
            off_distribution,
            rng,
        }
    }
}

impl<'sim, E> Component<'sim, E> for Toggler<'sim, E> {
    type Receive = Never;

    fn tick(&mut self, context: EffectContext) -> Vec<Message<'sim, E>> {
        assert_eq!(
            Some(context.time),
            Component::<E>::next_tick(self, context.time)
        );
        let mut effects = Vec::new();
        if context.time == self.next_toggle {
            self.enabled = !self.enabled;
            let dist = if self.enabled {
                effects.push(self.target.create_message(Toggle::Enable));
                &self.on_distribution
            } else {
                effects.push(self.target.create_message(Toggle::Disable));
                &self.off_distribution
            };
            self.next_toggle = context.time + self.rng.sample(dist);
        }
        effects
    }

    fn receive(&mut self, _e: Never, _context: EffectContext) -> Vec<Message<'sim, E>> {
        panic!()
    }

    fn next_tick(&self, _time: Time) -> Option<Time> {
        Some(self.next_toggle)
    }
}
