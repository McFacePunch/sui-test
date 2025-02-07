// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::collections::HashMap;

use super::execution::SimulateTransactionQueryParameters;
use super::TransactionSimulationResponse;
use crate::accept::AcceptFormat;
use crate::objects::ObjectNotFoundError;
use crate::openapi::ApiEndpoint;
use crate::openapi::OperationBuilder;
use crate::openapi::RequestBodyBuilder;
use crate::openapi::ResponseBuilder;
use crate::openapi::RouteHandler;
use crate::reader::StateReader;
use crate::response::ResponseContent;
use crate::RestError;
use crate::RestService;
use crate::Result;
use axum::extract::Query;
use axum::extract::State;
use axum::Json;
use itertools::Itertools;
use move_binary_format::normalized;
use schemars::JsonSchema;
use sui_protocol_config::ProtocolConfig;
use sui_sdk_types::types::Argument;
use sui_sdk_types::types::Command;
use sui_sdk_types::types::ObjectId;
use sui_sdk_types::types::Transaction;
use sui_sdk_types::types::UnresolvedInputArgument;
use sui_sdk_types::types::UnresolvedObjectReference;
use sui_sdk_types::types::UnresolvedProgrammableTransaction;
use sui_sdk_types::types::UnresolvedTransaction;
use sui_types::base_types::ObjectID;
use sui_types::base_types::ObjectRef;
use sui_types::base_types::SuiAddress;
use sui_types::effects::TransactionEffectsAPI;
use sui_types::gas::GasCostSummary;
use sui_types::gas_coin::GasCoin;
use sui_types::move_package::MovePackage;
use sui_types::transaction::CallArg;
use sui_types::transaction::GasData;
use sui_types::transaction::ObjectArg;
use sui_types::transaction::ProgrammableTransaction;
use sui_types::transaction::TransactionData;
use sui_types::transaction::TransactionDataAPI;
use tap::Pipe;

// TODO
// - Updating the UnresolvedTransaction format to provide less information about inputs
// - handle basic type inference and BCS serialization of pure args
pub struct ResolveTransaction;

impl ApiEndpoint<RestService> for ResolveTransaction {
    fn method(&self) -> axum::http::Method {
        axum::http::Method::POST
    }

    fn path(&self) -> &'static str {
        "/transactions/resolve"
    }

    fn operation(
        &self,
        generator: &mut schemars::gen::SchemaGenerator,
    ) -> openapiv3::v3_1::Operation {
        OperationBuilder::new()
            .tag("Transactions")
            .operation_id("ResolveTransaction")
            .query_parameters::<ResolveTransactionQueryParameters>(generator)
            .request_body(
                RequestBodyBuilder::new()
                    // .json_content::<UnresolvedTransaction>(generator)
                    .build(),
            )
            .response(
                200,
                ResponseBuilder::new()
                    .json_content::<ResolveTransactionResponse>(generator)
                    .bcs_content()
                    .build(),
            )
            .build()
    }

    fn handler(&self) -> RouteHandler<RestService> {
        RouteHandler::new(self.method(), resolve_transaction)
    }
}

async fn resolve_transaction(
    State(state): State<RestService>,
    Query(parameters): Query<ResolveTransactionQueryParameters>,
    accept: AcceptFormat,
    Json(unresolved_transaction): Json<UnresolvedTransaction>,
) -> Result<ResponseContent<ResolveTransactionResponse>> {
    let executor = state
        .executor
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No Transaction Executor"))?;
    let (reference_gas_price, protocol_config) = {
        let system_state = state.reader.get_system_state_summary()?;

        let current_protocol_version = state.reader.get_system_state_summary()?.protocol_version;

        let protocol_config = ProtocolConfig::get_for_version_if_supported(
            current_protocol_version.into(),
            state.reader.inner().get_chain_identifier()?.chain(),
        )
        .ok_or_else(|| {
            RestError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "unable to get current protocol config",
            )
        })?;

        (system_state.reference_gas_price, protocol_config)
    };
    let called_packages =
        called_packages(&state.reader, &protocol_config, &unresolved_transaction)?;
    let user_provided_budget = unresolved_transaction
        .gas_payment
        .as_ref()
        .and_then(|payment| payment.budget);
    let mut resolved_transaction = resolve_unresolved_transaction(
        &state.reader,
        &called_packages,
        reference_gas_price,
        protocol_config.max_tx_gas(),
        unresolved_transaction,
    )?;

    // If the user didn't provide a budget we need to run a quick simulation in order to calculate
    // a good estimated budget to use
    let budget = if let Some(user_provided_budget) = user_provided_budget {
        user_provided_budget
    } else {
        let simulation_result = executor
            .simulate_transaction(resolved_transaction.clone())
            .map_err(anyhow::Error::from)?;

        let estimate = estimate_gas_budget_from_gas_cost(
            simulation_result.effects.gas_cost_summary(),
            reference_gas_price,
        );
        resolved_transaction.gas_data_mut().budget = estimate;
        estimate
    };

    // If the user didn't provide any gas payment we need to do gas selection now
    if resolved_transaction.gas_data().payment.is_empty() {
        let input_objects = resolved_transaction
            .input_objects()
            .map_err(anyhow::Error::from)?
            .iter()
            .flat_map(|obj| match obj {
                sui_types::transaction::InputObjectKind::ImmOrOwnedMoveObject((id, _, _)) => {
                    Some(*id)
                }
                _ => None,
            })
            .collect_vec();
        let gas_coins = select_gas(
            &state.reader,
            resolved_transaction.gas_data().owner,
            budget,
            protocol_config.max_gas_payment_objects(),
            &input_objects,
        )?;
        resolved_transaction.gas_data_mut().payment = gas_coins;
    }

    let simulation = if parameters.simulate {
        super::execution::simulate_transaction_impl(
            executor,
            &parameters.simulate_transaction_parameters,
            resolved_transaction.clone().try_into()?,
        )?
        .pipe(Some)
    } else {
        None
    };

    ResolveTransactionResponse {
        transaction: resolved_transaction.try_into()?,
        simulation,
    }
    .pipe(|response| match accept {
        AcceptFormat::Json => ResponseContent::Json(response),
        AcceptFormat::Bcs => ResponseContent::Bcs(response),
    })
    .pipe(Ok)
}

/// Query parameters for the resolve transaction endpoint
#[derive(Debug, Default, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct ResolveTransactionQueryParameters {
    /// Request that the fully resolved transaction be simulated and have its results sent back in
    /// the response.
    #[serde(default)]
    pub simulate: bool,
    #[serde(flatten)]
    pub simulate_transaction_parameters: SimulateTransactionQueryParameters,
}

struct NormalizedPackage {
    #[allow(unused)]
    package: MovePackage,
    normalized_modules: BTreeMap<String, normalized::Module>,
}

fn called_packages(
    reader: &StateReader,
    protocol_config: &ProtocolConfig,
    unresolved_transaction: &UnresolvedTransaction,
) -> Result<HashMap<ObjectId, NormalizedPackage>> {
    let binary_config = sui_types::execution_config_utils::to_binary_config(protocol_config);
    let mut packages = HashMap::new();

    for move_call in unresolved_transaction
        .ptb
        .commands
        .iter()
        .filter_map(|command| {
            if let Command::MoveCall(move_call) = command {
                Some(move_call)
            } else {
                None
            }
        })
    {
        let package = reader
            .inner()
            .get_object(&(move_call.package.into()))?
            .ok_or_else(|| ObjectNotFoundError::new(move_call.package))?
            .data
            .try_as_package()
            .ok_or_else(|| {
                RestError::new(
                    axum::http::StatusCode::BAD_REQUEST,
                    format!("object {} is not a package", move_call.package),
                )
            })?
            .to_owned();

        // Normalization doesn't take the linkage or type origin tables into account, which means
        // that if you have an upgraded package that introduces a new type, then that type's
        // package ID is going to appear incorrectly if you fetch it from its normalized module.
        //
        // Despite the above this is safe given we are only using the signature information (and in
        // particular the reference kind) from the normalized package.
        let normalized_modules = package.normalize(&binary_config).map_err(|e| {
            RestError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("unable to normalize package {}: {e}", move_call.package),
            )
        })?;
        let package = NormalizedPackage {
            package,
            normalized_modules,
        };

        packages.insert(move_call.package, package);
    }

    Ok(packages)
}

fn resolve_unresolved_transaction(
    reader: &StateReader,
    called_packages: &HashMap<ObjectId, NormalizedPackage>,
    reference_gas_price: u64,
    max_gas_budget: u64,
    unresolved_transaction: UnresolvedTransaction,
) -> Result<TransactionData> {
    let sender = unresolved_transaction.sender.into();
    let gas_data = if let Some(unresolved_gas_payment) = unresolved_transaction.gas_payment {
        let payment = unresolved_gas_payment
            .objects
            .into_iter()
            .map(|unresolved| resolve_object_reference(reader, unresolved))
            .collect::<Result<Vec<_>>>()?;
        GasData {
            payment,
            owner: unresolved_gas_payment.owner.into(),
            price: unresolved_gas_payment.price.unwrap_or(reference_gas_price),
            budget: unresolved_gas_payment.budget.unwrap_or(max_gas_budget),
        }
    } else {
        GasData {
            payment: vec![],
            owner: sender,
            price: reference_gas_price,
            budget: max_gas_budget,
        }
    };
    let expiration = unresolved_transaction.expiration.into();
    let ptb = resolve_ptb(reader, called_packages, unresolved_transaction.ptb)?;
    Ok(TransactionData::V1(
        sui_types::transaction::TransactionDataV1 {
            kind: sui_types::transaction::TransactionKind::ProgrammableTransaction(ptb),
            sender,
            gas_data,
            expiration,
        },
    ))
}

/// Response type for the execute transaction endpoint
#[derive(Debug, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct ResolveTransactionResponse {
    pub transaction: Transaction,
    pub simulation: Option<TransactionSimulationResponse>,
}

fn resolve_object_reference(
    reader: &StateReader,
    unresolved_object_reference: UnresolvedObjectReference,
) -> Result<ObjectRef> {
    let UnresolvedObjectReference {
        object_id,
        version,
        digest,
    } = unresolved_object_reference;

    let id = object_id.into();
    let (v, d) = if let Some(version) = version {
        let object = reader
            .inner()
            .get_object_by_key(&id, version.into())?
            .ok_or_else(|| ObjectNotFoundError::new_with_version(object_id, version))?;
        (object.version(), object.digest())
    } else {
        let object = reader
            .inner()
            .get_object(&id)?
            .ok_or_else(|| ObjectNotFoundError::new(object_id))?;
        (object.version(), object.digest())
    };

    if digest.is_some_and(|digest| digest.inner() != d.inner()) {
        return Err(RestError::new(
            axum::http::StatusCode::BAD_REQUEST,
            format!("provided digest doesn't match, provided: {digest:?} actual: {d}"),
        ));
    }

    Ok((id, v, d))
}

fn resolve_ptb(
    reader: &StateReader,
    called_packages: &HashMap<ObjectId, NormalizedPackage>,
    unresolved_ptb: UnresolvedProgrammableTransaction,
) -> Result<ProgrammableTransaction> {
    let inputs = unresolved_ptb
        .inputs
        .into_iter()
        .enumerate()
        .map(|(arg_idx, arg)| {
            resolve_arg(
                reader,
                called_packages,
                &unresolved_ptb.commands,
                arg,
                arg_idx,
            )
        })
        .collect::<Result<_>>()?;

    ProgrammableTransaction {
        inputs,
        commands: unresolved_ptb
            .commands
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<_, _>>()?,
    }
    .pipe(Ok)
}

fn resolve_arg(
    reader: &StateReader,
    called_packages: &HashMap<ObjectId, NormalizedPackage>,
    commands: &[Command],
    arg: UnresolvedInputArgument,
    arg_idx: usize,
) -> Result<CallArg> {
    match arg {
        UnresolvedInputArgument::Pure { value } => CallArg::Pure(value),
        UnresolvedInputArgument::ImmutableOrOwned(obj_ref) => CallArg::Object(
            ObjectArg::ImmOrOwnedObject(resolve_object_reference(reader, obj_ref)?),
        ),
        UnresolvedInputArgument::Shared {
            object_id,
            initial_shared_version: _,
            mutable: _,
        } => {
            let id = object_id.into();
            let object = reader
                .inner()
                .get_object(&id)?
                .ok_or_else(|| ObjectNotFoundError::new(object_id))?;

            let initial_shared_version = if let sui_types::object::Owner::Shared {
                initial_shared_version,
            } = object.owner()
            {
                *initial_shared_version
            } else {
                return Err(RestError::new(
                    axum::http::StatusCode::BAD_REQUEST,
                    format!("object {object_id} is not a shared object"),
                ));
            };

            let mut mutable = false;

            for (command, idx) in find_arg_uses(arg_idx, commands) {
                match (command, idx) {
                    (Command::MoveCall(move_call), Some(idx)) => {
                        let function = called_packages
                            // Find the package
                            .get(&move_call.package)
                            // Find the module
                            .and_then(|package| {
                                package.normalized_modules.get(move_call.module.as_str())
                            })
                            // Find the function
                            .and_then(|module| module.functions.get(move_call.function.as_str()))
                            .ok_or_else(|| {
                                RestError::new(
                                    axum::http::StatusCode::BAD_REQUEST,
                                    format!(
                                        "unable to find function {package}::{module}::{function}",
                                        package = move_call.package,
                                        module = move_call.module,
                                        function = move_call.function
                                    ),
                                )
                            })?;

                        let arg_type = function.parameters.get(idx).ok_or_else(|| {
                            RestError::new(
                                axum::http::StatusCode::BAD_REQUEST,
                                "invalid input parameter",
                            )
                        })?;

                        if matches!(
                            arg_type,
                            move_binary_format::normalized::Type::MutableReference(_)
                                | move_binary_format::normalized::Type::Struct { .. }
                        ) {
                            mutable = true;
                        }
                    }

                    (
                        Command::SplitCoins(_)
                        | Command::MergeCoins(_)
                        | Command::MakeMoveVector(_),
                        _,
                    ) => {
                        mutable = true;
                    }

                    _ => {}
                }

                // Early break out of the loop if we've already determined that the shared object
                // is needed to be mutable
                if mutable {
                    break;
                }
            }

            CallArg::Object(ObjectArg::SharedObject {
                id,
                initial_shared_version,
                mutable,
            })
        }
        UnresolvedInputArgument::Receiving(obj_ref) => CallArg::Object(ObjectArg::Receiving(
            resolve_object_reference(reader, obj_ref)?,
        )),
    }
    .pipe(Ok)
}

/// Given an particular input argument, find all of its uses.
///
/// The returned iterator contains all commands where the argument is used and an optional index
/// for where the argument is used in that command.
fn find_arg_uses(
    arg_idx: usize,
    commands: &[Command],
) -> impl Iterator<Item = (&Command, Option<usize>)> {
    commands.iter().filter_map(move |command| {
        match command {
            Command::MoveCall(move_call) => move_call
                .arguments
                .iter()
                .position(|elem| matches_input_arg(*elem, arg_idx))
                .map(Some),
            Command::TransferObjects(transfer_objects) => transfer_objects
                .objects
                .iter()
                .position(|elem| matches_input_arg(*elem, arg_idx))
                .map(Some),
            Command::SplitCoins(split_coins) => {
                matches_input_arg(split_coins.coin, arg_idx).then_some(None)
            }
            Command::MergeCoins(merge_coins) => {
                if matches_input_arg(merge_coins.coin, arg_idx) {
                    Some(None)
                } else {
                    merge_coins
                        .coins_to_merge
                        .iter()
                        .position(|elem| matches_input_arg(*elem, arg_idx))
                        .map(Some)
                }
            }
            Command::Publish(_) => None,
            Command::MakeMoveVector(make_move_vector) => make_move_vector
                .elements
                .iter()
                .position(|elem| matches_input_arg(*elem, arg_idx))
                .map(Some),
            Command::Upgrade(upgrade) => matches_input_arg(upgrade.ticket, arg_idx).then_some(None),
        }
        .map(|x| (command, x))
    })
}

fn matches_input_arg(arg: Argument, arg_idx: usize) -> bool {
    matches!(arg, Argument::Input(idx) if idx as usize == arg_idx)
}

/// Estimate the gas budget using the gas_cost_summary from a previous DryRun
///
/// The estimated gas budget is computed as following:
/// * the maximum between A and B, where:
///     A = computation cost + GAS_SAFE_OVERHEAD * reference gas price
///     B = computation cost + storage cost - storage rebate + GAS_SAFE_OVERHEAD * reference gas price
///     overhead
///
/// This gas estimate is computed similarly as in the TypeScript SDK
fn estimate_gas_budget_from_gas_cost(
    gas_cost_summary: &GasCostSummary,
    reference_gas_price: u64,
) -> u64 {
    const GAS_SAFE_OVERHEAD: u64 = 1000;

    let safe_overhead = GAS_SAFE_OVERHEAD * reference_gas_price;
    let computation_cost_with_overhead = gas_cost_summary.computation_cost + safe_overhead;

    let gas_usage = gas_cost_summary.net_gas_usage() + safe_overhead as i64;
    computation_cost_with_overhead.max(if gas_usage < 0 { 0 } else { gas_usage as u64 })
}

fn select_gas(
    reader: &StateReader,
    owner: SuiAddress,
    budget: u64,
    max_gas_payment_objects: u32,
    input_objects: &[ObjectID],
) -> Result<Vec<ObjectRef>> {
    let gas_coins = reader
        .inner()
        .account_owned_objects_info_iter(owner, None)?
        .filter(|info| info.type_.is_gas_coin())
        .filter(|info| !input_objects.contains(&info.object_id))
        .filter_map(|info| reader.inner().get_object(&info.object_id).ok().flatten())
        .filter_map(|object| {
            GasCoin::try_from(&object)
                .ok()
                .map(|coin| (object.compute_object_reference(), coin.value()))
        })
        .take(max_gas_payment_objects as usize);

    let mut selected_gas = vec![];
    let mut selected_gas_value = 0;

    for (object_ref, value) in gas_coins {
        selected_gas.push(object_ref);
        selected_gas_value += value;
    }

    if selected_gas_value >= budget {
        Ok(selected_gas)
    } else {
        Err(RestError::new(
            axum::http::StatusCode::BAD_REQUEST,
            format!(
                "unable to select sufficient gas coins from account {owner} \
                    to satisfy required budget {budget}"
            ),
        ))
    }
}
