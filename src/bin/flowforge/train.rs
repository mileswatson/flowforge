use std::path::Path;

use anyhow::Result;
use flowforge::{
    flow::UtilityConfig,
    network::config::NetworkConfig,
    rand::Rng,
    trainers::{
        delay_multiplier::{DelayMultiplierDna, DelayMultiplierTrainer},
        remy::{RemyDna, RemyTrainer},
        TrainerConfig,
    },
    Config, Trainer,
};

pub fn train(
    trainer_config: &Path,
    network_config: &Path,
    utility_config: &Path,
    output: &Path,
) -> Result<()> {
    let trainer_config = TrainerConfig::load(trainer_config)?;
    let network_config = NetworkConfig::load(network_config)?;
    let utility_config = UtilityConfig::load(utility_config)?;

    let mut rng = Rng::from_seed(0);

    match trainer_config {
        TrainerConfig::Remy(cfg) => {
            RemyTrainer::new(&cfg).train(
                &network_config,
                utility_config.inner(),
                &mut |progress, d: Option<&RemyDna>| {},
                &mut rng,
            );
        }
        TrainerConfig::DelayMultiplier(cfg) => {
            DelayMultiplierTrainer::new(&cfg).train(
                &network_config,
                utility_config.inner(),
                &mut |progress, d: Option<&DelayMultiplierDna>| {
                    if let Some(d) = d {
                        println!("{} {:?}", progress, d);
                    }
                },
                &mut rng,
            );
        }
    };

    Ok(())
}
