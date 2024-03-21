use generativity::make_guard;
use itertools::Itertools;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use serde::{Deserialize, Serialize};

use crate::{
    average::{AveragePair, IterAverage, SameEmptiness},
    flow::{FlowProperties, NoActiveFlows, UtilityFunction},
    network::{
        config::NetworkConfig, toggler::Toggle, EffectTypeGenerator, Network, Packet,
        PopulateComponents,
    },
    never::Never,
    quantities::{seconds, Float, Time, TimeSpan},
    rand::Rng,
    simulation::HasSubEffect,
};

#[allow(clippy::unsafe_derive_deserialize)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationConfig {
    pub network_samples: u32,
    pub run_sim_for: TimeSpan,
}

impl Default for EvaluationConfig {
    fn default() -> Self {
        Self {
            network_samples: 1000,
            run_sim_for: seconds(120.),
        }
    }
}

impl EvaluationConfig {
    pub fn evaluate<G>(
        &self,
        network_config: &NetworkConfig,
        components: &impl PopulateComponents<G>,
        utility_function: &(impl UtilityFunction + ?Sized),
        rng: &mut Rng,
    ) -> Result<(Float, FlowProperties), NoActiveFlows>
    where
        G: EffectTypeGenerator,
        for<'sim> G::Type<'sim>:
            HasSubEffect<Packet<'sim, G::Type<'sim>>> + HasSubEffect<Toggle> + HasSubEffect<Never>,
    {
        let score_network = |(n, mut rng): (Network, Rng)| {
            make_guard!(guard);
            let (sim, flows) = n.to_sim(guard, &mut rng, components);
            sim.run_for(self.run_sim_for);
            utility_function.total_utility(&flows, Time::SIM_START + self.run_sim_for)
        };

        let networks = (0..self.network_samples)
            .map(|_| (rng.sample(network_config), rng.create_child()))
            .collect_vec();
        networks
            .into_par_iter()
            .map(score_network)
            .filter_map(Result::ok)
            .map(AveragePair::new)
            .collect::<Vec<_>>()
            .average()
            .assert_same_emptiness()
            .map_err(|_| NoActiveFlows)
    }
}
