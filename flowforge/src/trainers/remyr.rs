use std::{cell::RefCell, f32::consts::PI, iter::once};

use dfdx::{data::IteratorBatchExt, prelude::*};
use generativity::make_guard;
use itertools::Itertools;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use serde::{Deserialize, Serialize};

use crate::{
    core::{
        meters::CurrentFlowMeter,
        rand::{ContinuousDistribution, Rng},
    },
    evaluator::EvaluationConfig,
    flow::UtilityFunction,
    network::config::NetworkConfig,
    protocols::{
        remy::{action::Action, point::Point, rule_tree::RuleTree},
        remyr::{
            dna::RemyrDna,
            net::{
                CopyToDevice, HiddenLayers, PolicyNet, PolicyNetwork, ACTION, OBSERVATION, STATE,
            },
        },
    },
    quantities::{milliseconds, seconds, Float, Time, TimeSpan},
    Trainer,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiscountingMode {
    Discrete { gamma: f32 },
    DiscreteDelta { gamma: f32 },
    DiscreteRate { gamma: f32 },
    ContinuousRate { half_life: TimeSpan },
}

use super::{remy::RemyFlowAdder, DefaultEffect};

#[derive(Clone, Serialize, Deserialize)]
pub struct RemyrConfig {
    pub iters: u32,
    pub updates_per_iter: u32,
    pub min_point: Point,
    pub max_point: Point,
    pub min_action: Action,
    pub max_action: Action,
    pub hidden_layers: HiddenLayers,
    pub learning_rate: f64,
    pub learning_rate_annealing: bool,
    pub clip: f32,
    pub clip_annealing: bool,
    pub weight_decay: Option<f64>,
    pub discounting_mode: DiscountingMode,
    pub bandwidth_half_life: TimeSpan,
    pub rollout_config: EvaluationConfig,
}

impl Default for RemyrConfig {
    fn default() -> Self {
        Self {
            iters: 1500,
            updates_per_iter: 5,
            min_point: Point {
                ack_ewma: milliseconds(0.),
                send_ewma: milliseconds(0.),
                rtt_ratio: 1.,
            },
            max_point: Point {
                ack_ewma: seconds(0.125),
                send_ewma: seconds(0.125),
                rtt_ratio: 5.,
            },
            min_action: Action {
                window_multiplier: 0.,
                window_increment: 0,
                intersend_delay: milliseconds(0.25),
            },
            max_action: Action {
                window_multiplier: 1.,
                window_increment: 256,
                intersend_delay: milliseconds(3.),
            },
            rollout_config: EvaluationConfig {
                network_samples: 8,
                run_sim_for: seconds(60.),
            },
            hidden_layers: HiddenLayers(64, 32),
            learning_rate: 0.0003,
            learning_rate_annealing: true,
            weight_decay: Some(0.001),
            bandwidth_half_life: milliseconds(100.),
            clip: 0.2,
            clip_annealing: true,
            discounting_mode: DiscountingMode::ContinuousRate {
                half_life: seconds(1.),
            },
        }
    }
}

impl RemyrConfig {
    fn initial_dna(&self, policy: PolicyNetwork<Cpu>) -> RemyrDna {
        RemyrDna {
            min_point: self.min_point.clone(),
            max_point: self.max_point.clone(),
            min_action: self.min_action.clone(),
            max_action: self.max_action.clone(),
            policy,
        }
    }
}

#[derive(Debug)]
struct Record {
    observation: [f32; OBSERVATION],
    action: [f32; ACTION],
    action_log_prob: f32,
    num_senders: usize,
}

#[derive(Debug)]
struct Trajectory {
    records: Vec<Record>,
    rewards_to_go_before_actions: Vec<f32>,
}

impl DiscountingMode {
    fn create_trajectory(&self, records: Vec<Record>, utilities: &[(f32, Time)]) -> Trajectory {
        assert_eq!(records.len() + 1, utilities.len());
        let utilities_after_action = &utilities[1..];
        let utilities_before_action = &utilities[..utilities.len() - 1];
        let mut rewards_to_go_before_actions = match self {
            DiscountingMode::Discrete { gamma } => utilities_after_action
                .iter()
                .rev()
                .scan(0., |acc, utility_after_action| {
                    *acc = utility_after_action.0 + gamma * *acc;
                    Some(*acc)
                })
                .collect_vec(),
            DiscountingMode::DiscreteDelta { gamma } => utilities_after_action
                .iter()
                .zip(utilities_before_action)
                .map(|(after, before)| after.0 - before.0)
                .rev()
                .scan(0., |acc, utility_delta| {
                    *acc = utility_delta + gamma * *acc;
                    Some(*acc)
                })
                .collect_vec(),
            DiscountingMode::DiscreteRate { gamma } => utilities_after_action
                .iter()
                .zip(utilities_before_action)
                .map(|(after, before)| after.0 * (after.1 - before.1).seconds() as f32)
                .rev()
                .scan(0., |acc, utility_delta| {
                    *acc = utility_delta + gamma * *acc;
                    Some(*acc)
                })
                .collect_vec(),
            DiscountingMode::ContinuousRate { half_life } => {
                let alpha = (2_f32).ln() / half_life.seconds() as f32;
                utilities_after_action
                    .iter()
                    .zip(utilities_before_action)
                    .map(|(after, before)| ((after.1 - before.1).seconds() as f32, after.0))
                    .rev()
                    .scan(0., |acc, (delta_t, utility_after_action)| {
                        let gamma = (-alpha * delta_t).exp();
                        *acc = alpha * (1. - gamma) * utility_after_action + gamma * *acc;
                        Some(*acc)
                    })
                    .collect_vec()
            }
        };
        rewards_to_go_before_actions.reverse();
        Trajectory {
            records,
            rewards_to_go_before_actions,
        }
    }
}

struct RolloutResult<D: Device<f32>> {
    states: Tensor<(usize, Const<STATE>), f32, D>,
    actions: Tensor<(usize, Const<ACTION>), f32, D>,
    action_log_probs: Tensor<(usize,), f32, D>,
    rewards_to_go_before_action: Tensor<(usize,), f32, D>,
}

impl<D: Device<f32>> RolloutResult<D> {
    pub fn new(trajectories: &[Trajectory], dev: &D) -> Self {
        let num_timesteps = trajectories.iter().map(|x| x.records.len()).sum();
        let observations = trajectories
            .iter()
            .flat_map(|x| x.records.iter())
            .flat_map(|x| {
                x.observation
                    .into_iter()
                    .chain(once(1. / x.num_senders as f32))
            })
            .collect();
        let actions = trajectories
            .iter()
            .flat_map(|x| x.records.iter())
            .flat_map(|x| x.action)
            .collect();
        let action_log_probs = trajectories
            .iter()
            .flat_map(|x| x.records.iter())
            .map(|x| x.action_log_prob)
            .collect();
        let rewards_to_go_before_action = trajectories
            .iter()
            .flat_map(|x| x.rewards_to_go_before_actions.iter())
            .copied()
            .collect();
        RolloutResult {
            states: dev.tensor_from_vec(observations, (num_timesteps, Const::<STATE>)),
            actions: dev.tensor_from_vec(actions, (num_timesteps, Const::<ACTION>)),
            action_log_probs: dev.tensor_from_vec(action_log_probs, (num_timesteps,)),
            rewards_to_go_before_action: dev
                .tensor_from_vec(rewards_to_go_before_action, (num_timesteps,)),
        }
    }
}

fn calculate_action_log_probs<S: Dim, D: Device<f32>, T: Tape<f32, D>>(
    actions: Tensor<(S, Const<ACTION>), f32, D>,
    means: Tensor<(S, Const<ACTION>), f32, D, T>,
    stddevs: Tensor<(S, Const<ACTION>), f32, D, T>,
) -> Tensor<(S,), f32, D, T> {
    (((means - actions) / stddevs.with_empty_tape()).square() + stddevs.ln() * 2. + (2. * PI).ln())
        .sum::<(S,), Axis<1>>()
        * -0.5
}

pub struct RolloutWrapper<'a, F, S> {
    num_senders: S,
    dna: &'a RemyrDna,
    rng: RefCell<&'a mut Rng>,
    prob_deterministic: Float,
    f: F,
}

impl<'a, F, S> std::fmt::Debug for RolloutWrapper<'a, F, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RolloutWrapper")
            .field("dna", &self.dna)
            .field("rng", &self.rng)
            .field("prob_deterministic", &self.prob_deterministic)
            .finish()
    }
}

impl<'a, F, S> RuleTree for RolloutWrapper<'a, F, S>
where
    F: Fn(Record, Time),
    S: Fn() -> usize,
{
    fn action(&self, point: &Point, time: Time) -> Option<Action> {
        Some(self.dna.raw_action(point, |observation, (mean, stddev)| {
            let mut rng = self.rng.borrow_mut();
            if rng.sample(&ContinuousDistribution::Uniform { min: 0., max: 1. })
                <= self.prob_deterministic
            {
                mean
            } else {
                let dev = self.dna.policy.device();
                let mut sample_normal = || {
                    rng.sample(&ContinuousDistribution::Normal {
                        mean: 0.,
                        std_dev: 1.,
                    }) as f32
                };
                let normal_samples =
                    dev.tensor([sample_normal(), sample_normal(), sample_normal()]);
                let action = mean.clone() + normal_samples * stddev.clone();
                let action_log_prob = calculate_action_log_probs::<Const<1>, _, _>(
                    action.clone().reshape(),
                    mean.reshape(),
                    stddev.reshape(),
                );
                (self.f)(
                    Record {
                        observation: observation.array(),
                        action: action.array(),
                        action_log_prob: action_log_prob.reshape::<()>().array(),
                        num_senders: (self.num_senders)(),
                    },
                    time,
                );
                action
            }
        }))
    }
}

fn rollout(
    dna: &RemyrDna,
    network_config: &NetworkConfig,
    utility_function: &dyn UtilityFunction,
    training_config: &EvaluationConfig,
    half_life: TimeSpan,
    discounting_mode: &DiscountingMode,
    steps: usize,
    prob_deterministic: Float,
    repeat: usize,
    rng: &mut Rng,
) -> Vec<Trajectory> {
    let networks = (0..training_config.network_samples)
        .map(|_| (rng.sample(network_config), rng.create_child()))
        .collect_vec();

    networks
        .into_par_iter()
        .map(|(n, mut rng)| {
            let records = RefCell::new((Vec::new(), Vec::new()));
            make_guard!(guard);
            let flows = (0..n.num_senders)
                .map(|_| RefCell::new(CurrentFlowMeter::new_disabled(Time::SIM_START, half_life)))
                .collect_vec();
            let current_utility = |time| {
                let flow_stats = flows
                    .iter()
                    .filter_map(|x| x.borrow().current_properties(time).ok())
                    .collect_vec();
                utility_function
                    .total_utility(&flow_stats)
                    .map(|(u, _)| u)
                    .unwrap_or(0.) as f32
            };
            let mut policy_rng = rng.create_child();
            let dna = RolloutWrapper {
                dna,
                f: |rec, time| {
                    let mut records = records.borrow_mut();
                    records.0.push(rec);
                    records.1.push((current_utility(time), time));
                },
                rng: RefCell::new(&mut policy_rng),
                prob_deterministic,
                num_senders: || flows.iter().filter(|x| x.borrow().active()).count(),
            };
            let sim = n.to_sim::<_, DefaultEffect>(
                &RemyFlowAdder::new(repeat),
                guard,
                &mut rng,
                &flows,
                &dna,
                |_| {},
            );
            let sim_end = sim.run_while(|_| records.borrow().0.len() < steps);
            let mut records = records.into_inner();
            records.1.push((current_utility(sim_end), sim_end));
            discounting_mode.create_trajectory(records.0, &records.1)
        })
        .collect()
}

pub struct RemyrTrainer {
    config: RemyrConfig,
}

impl Trainer for RemyrTrainer {
    type Config = RemyrConfig;
    type Dna = RemyrDna;
    type DefaultEffectGenerator = DefaultEffect<'static>;
    type DefaultFlowAdder<'a> = RemyFlowAdder;

    fn new(config: &Self::Config) -> Self {
        RemyrTrainer {
            config: config.clone(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn train<H>(
        &self,
        starting_point: Option<Self::Dna>,
        network_config: &NetworkConfig,
        utility_function: &dyn UtilityFunction,
        progress_handler: &mut H,
        rng: &mut crate::core::rand::Rng,
    ) -> Self::Dna
    where
        H: crate::ProgressHandler<Self::Dna>,
    {
        assert!(
            starting_point.is_none(),
            "Starting point not supported for genetic trainer!"
        );
        let dev = AutoDevice::default();
        let mut policy = dev.build_module(self.config.hidden_layers.policy_arch());
        let mut critic = dev.build_module::<f32>(self.config.hidden_layers.critic_arch());

        let mut critic_gradients = critic.alloc_grads();
        let mut critic_optimizer = Adam::new(
            &critic,
            AdamConfig {
                lr: self.config.learning_rate,
                weight_decay: self.config.weight_decay.map(WeightDecay::Decoupled),
                eps: 1e-5,
                ..Default::default()
            },
        );
        let mut policy_gradients = policy.alloc_grads();
        let mut policy_optimizer = Adam::new(
            &policy,
            AdamConfig {
                lr: self.config.learning_rate,
                weight_decay: self.config.weight_decay.map(WeightDecay::Decoupled),
                eps: 1e-5,
                ..Default::default()
            },
        );

        let sim_dev = Cpu::default();

        for i in 0..self.config.iters {
            let dna = self.config.initial_dna(policy.copy_to(&sim_dev));

            let frac = f64::from(i) / f64::from(self.config.iters);
            progress_handler.update_progress(frac, &dna);

            if self.config.learning_rate_annealing {
                policy_optimizer.cfg.lr = (1.0 - frac) * self.config.learning_rate;
                critic_optimizer.cfg.lr = (1.0 - frac) * self.config.learning_rate;
            }

            let clip = if self.config.clip_annealing {
                (1.0 - frac as f32) * self.config.clip
            } else {
                self.config.clip
            };

            let trajectories: Vec<Trajectory> = rollout(
                &dna,
                network_config,
                utility_function,
                &self.config.rollout_config,
                self.config.bandwidth_half_life,
                &self.config.discounting_mode,
                2048,
                frac * 0.99,
                (100. * (1. - frac)).round() as usize,
                rng,
            );
            let RolloutResult {
                states,
                actions,
                action_log_probs,
                rewards_to_go_before_action,
            } = RolloutResult::new(&trajectories, &dev);
            let estimated_values_k = critic.forward(states.clone()); // V

            let advantages_k = {
                let shape = (estimated_values_k.shape().0,);
                rewards_to_go_before_action.clone() - estimated_values_k.reshape_like(&shape)
            };

            for _ in 0..self.config.updates_per_iter {
                let mut all_indices = (0..states.shape().0).collect_vec();
                let num_batches = 4;
                let batch_size = all_indices.len() / num_batches;
                rng.shuffle(&mut all_indices);
                for batch_indices in all_indices.into_iter().batch_with_last(batch_size) {
                    let batch_len = batch_indices.len();
                    let batch_indices = dev.tensor_from_vec(batch_indices, (batch_len,));

                    let batch_states = states.clone().gather(batch_indices.clone());
                    let batch_observations = batch_states
                        .clone()
                        .slice((.., ..OBSERVATION))
                        .reshape_like(&(batch_len, Const::<OBSERVATION>));

                    let (batch_means, batch_stddevs) =
                        policy.forward(batch_observations.clone().trace(policy_gradients));

                    let batch_action_log_probs = calculate_action_log_probs(
                        actions.clone().gather(batch_indices.clone()),
                        batch_means,
                        batch_stddevs,
                    );

                    let batch_ratios = (batch_action_log_probs
                        - action_log_probs.clone().gather(batch_indices.clone()))
                    .exp();

                    let batch_advantages = advantages_k.clone().gather(batch_indices.clone());
                    let batch_advantages = (batch_advantages.clone()
                        - batch_advantages.clone().mean().array())
                        / (batch_advantages.stddev(0.).array() + 1e-10);

                    let policy_loss = (-minimum(
                        batch_ratios.with_empty_tape() * batch_advantages.clone(),
                        clamp(batch_ratios, 1. - clip, 1. + clip) * batch_advantages.clone(),
                    ))
                    .sum();

                    policy_gradients = policy_loss.backward();

                    policy_optimizer
                        .update(&mut policy, &policy_gradients)
                        .unwrap();
                    policy.zero_grads(&mut policy_gradients);

                    // critic
                    let batch_estimated_values =
                        critic.forward(batch_states.clone().traced(critic_gradients));

                    let batch_rewards_to_go_before_action =
                        rewards_to_go_before_action.clone().gather(batch_indices);

                    let critic_loss = mse_loss(
                        batch_estimated_values
                            .reshape_like(batch_rewards_to_go_before_action.shape()),
                        batch_rewards_to_go_before_action.clone(),
                    );

                    critic_gradients = critic_loss.backward();
                    critic_optimizer
                        .update(&mut critic, &critic_gradients)
                        .unwrap();
                    critic.zero_grads(&mut critic_gradients);
                }
            }
        }
        let dna = self.config.initial_dna(policy.copy_to(&sim_dev));
        progress_handler.update_progress(1., &dna);
        dna
    }
}
