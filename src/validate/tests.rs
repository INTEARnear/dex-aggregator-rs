use std::collections::BTreeMap;

use base64::{Engine, prelude::BASE64_STANDARD};
use serde_json::{Value, json};

use super::{UnsafeTransactionsError, check_next_steps, check_quote};
use crate::{
    request::{Slippage, SwapAmount, SwapRequest},
    sender::Transaction,
    types::{Quote, SwapOutcome},
};

const USDC: &str = "17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1";
const USDT: &str = "nep141:usdt.tether-token.near";
const ONE_NEAR: u128 = 10u128.pow(24);
const FIXTURES: [&str; 6] = [
    "near_to_usdc_exact_in",
    "usdc_to_near_exact_in",
    "near_to_usdc_exact_out",
    "usdc_to_near_exact_out",
    "rhea_balances_exact_in",
    "intear_balances_exact_in",
];

/// What each fixture's quotes were requested with, from router.intear.tech
fn request_of(fixture: &str) -> SwapRequest {
    let (token_in, token_out, amount) = match fixture {
        "near_to_usdc_exact_in" => ("near", USDC.to_string(), SwapAmount::ExactIn(ONE_NEAR)),
        "usdc_to_near_exact_in" => (USDC, "near".to_string(), SwapAmount::ExactIn(3_000_000)),
        "near_to_usdc_exact_out" => ("near", USDC.to_string(), SwapAmount::ExactOut(2_000_000)),
        "usdc_to_near_exact_out" => (USDC, "near".to_string(), SwapAmount::ExactOut(ONE_NEAR)),
        "rhea_balances_exact_in" => (
            "rhea-nep141:wrap.near",
            format!("rhea-nep141:{USDC}"),
            SwapAmount::ExactIn(ONE_NEAR),
        ),
        "intear_balances_exact_in" => (
            "intear-dex:near",
            format!("intear-dex:nep141:{USDC}"),
            SwapAmount::ExactIn(ONE_NEAR),
        ),
        _ => panic!("No fixture {fixture}"),
    };
    SwapRequest {
        referrer_id: Some("wallet.intear.near".parse().unwrap()),
        fees: BTreeMap::from([("wallet.intear.near".parse().unwrap(), 0.005)]),
        quote_improvement_receiver: Some("wallet.intear.near".parse().unwrap()),
        ..SwapRequest::new(
            token_in.parse().unwrap(),
            token_out.parse().unwrap(),
            amount,
            Slippage::Fixed(0.01),
            "intear.near".parse().unwrap(),
        )
    }
}

/// A quote of router.intear.tech, with its fee transactions
fn fixture_quote(fixture: &str, dex_id: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/{fixture}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let quotes: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    quotes
        .into_iter()
        .find(|quote| quote["dex_id"] == dex_id)
        .unwrap_or_else(|| panic!("{fixture} has no {dex_id} quote"))
}

fn is_fee_transaction(instruction: &Value) -> bool {
    instruction["NearTransaction"]["is_fee_transaction"] == true
}

/// The quote as a subscription with its fees in the next steps has it: without its fee
/// transactions, with an ID
fn quote_json(fixture: &str, dex_id: &str) -> Value {
    let mut quote = fixture_quote(fixture, dex_id);
    let instructions = quote["execution_instructions"].as_array_mut().unwrap();
    instructions.retain(|instruction| !is_fee_transaction(instruction));
    quote["quote_id"] = json!(7);
    quote
}

fn fee_transactions(fixture: &str, dex_id: &str) -> Vec<Value> {
    let quote = fixture_quote(fixture, dex_id);
    let instructions = quote["execution_instructions"].as_array().unwrap();
    instructions
        .iter()
        .filter(|instruction| is_fee_transaction(instruction))
        .cloned()
        .collect()
}

fn quote(json: Value) -> Quote {
    serde_json::from_value(json).unwrap()
}

fn encode(args: &Value) -> Value {
    BASE64_STANDARD
        .encode(serde_json::to_vec(args).unwrap())
        .into()
}

fn call(method: &str, args: Value, deposit: &str, gas: u64) -> Value {
    json!({ "FunctionCall": { "method_name": method, "args": encode(&args), "gas": gas, "deposit": deposit } })
}

fn one_yocto_call(method: &str, args: Value) -> Value {
    call(method, args, "1", 30_000_000_000_000)
}

fn transaction(receiver_id: &str, actions: Vec<Value>, is_fee_transaction: bool) -> Value {
    json!({ "NearTransaction": { "receiver_id": receiver_id, "actions": actions, "is_fee_transaction": is_fee_transaction } })
}

/// A copy of the quote with an action's arguments, or the action itself, changed
fn tamper(
    mut quote: Value,
    instruction: usize,
    action: usize,
    change: impl FnOnce(&mut Value, &mut Value),
) -> Value {
    let call = &mut quote["execution_instructions"][instruction]["NearTransaction"]["actions"]
        [action]["FunctionCall"];
    let mut args: Value = serde_json::from_slice(
        &BASE64_STANDARD
            .decode(call["args"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    change(&mut args, call);
    call["args"] = encode(&args);
    quote
}

/// Changes a JSON string field of the arguments, like an `ft_transfer_call`'s `msg`
fn tamper_json(args: &mut Value, field: &str, change: impl FnOnce(&mut Value)) {
    let mut value: Value = serde_json::from_str(args[field].as_str().unwrap()).unwrap();
    change(&mut value);
    args[field] = value.to_string().into();
}

fn problems(checked: Result<Vec<Transaction>, UnsafeTransactionsError>) -> Vec<String> {
    checked
        .err()
        .map(|error| error.problems)
        .unwrap_or_default()
}

fn quote_problems(fixture: &str, json: Value) -> Vec<String> {
    problems(check_quote(&request_of(fixture), &quote(json)))
}

#[track_caller]
fn assert_unsafe(fixture: &str, json: Value, problem: &str) {
    let problems = quote_problems(fixture, json);
    assert!(
        problems.iter().any(|found| found.contains(problem)),
        "{problem:?} isn't one of {problems:#?}"
    );
}

/// Of a `deposit_near` to Intear DEX
fn engine_operations(args: &mut Value) -> &mut Vec<Value> {
    args["operations"]["operations"].as_array_mut().unwrap()
}

fn engine_operation<'a>(args: &'a mut Value, kind: &str) -> &'a mut Value {
    engine_operations(args)
        .iter_mut()
        .find_map(|operation| operation.get_mut(kind))
        .unwrap_or_else(|| panic!("No {kind} operation"))
}

#[test]
fn quotes_of_the_router_are_safe_to_sign() {
    for fixture in FIXTURES {
        let path = format!(
            "{}/tests/fixtures/{fixture}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let quotes: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for dex_id in quotes.iter().map(|quote| quote["dex_id"].as_str().unwrap()) {
            let problems = quote_problems(fixture, quote_json(fixture, dex_id));
            assert_eq!(problems, Vec::<String>::new(), "{fixture} on {dex_id}");
        }
    }
}

#[test]
fn checked_transactions_have_the_quotes_calls() {
    let transactions = check_quote(
        &request_of("near_to_usdc_exact_in"),
        &quote(quote_json("near_to_usdc_exact_in", "RheaDcl")),
    )
    .unwrap();
    assert_eq!(transactions.len(), 1);
    assert_eq!(transactions[0].receiver_id.as_str(), "wrap.near");
    let [near_deposit, ft_transfer_call] = transactions[0].actions.as_slice() else {
        panic!("Not two actions");
    };
    assert_eq!(near_deposit.method_name, "near_deposit");
    assert_eq!(near_deposit.args, "{}");
    assert_eq!(near_deposit.deposit.as_yoctonear(), ONE_NEAR);
    assert_eq!(ft_transfer_call.gas.as_gas(), 105_348_000_000_000);
    let args: Value = serde_json::from_str(&ft_transfer_call.args).unwrap();
    assert_eq!(args["receiver_id"], "dclv2.ref-labs.near");
}

#[test]
fn a_quote_with_fee_transactions_is_of_another_api() {
    let mut json = fixture_quote("near_to_usdc_exact_in", "RheaDcl");
    json["quote_id"] = json!(7);
    assert_unsafe(
        "near_to_usdc_exact_in",
        json,
        "A quote whose fees are in the next steps has fee transactions",
    );
}

#[test]
fn buying_the_oracle_fee_with_the_input() {
    let json = tamper(
        quote_json("near_to_usdc_exact_in", "IntearDex"),
        0,
        0,
        |args, _| {
            let oracle_request = engine_operation(args, "OracleRequest");
            let near_fee = oracle_request["fee"][1].clone();
            oracle_request["fee"] = json!([format!("nep141:{USDC}"), "1000"]);
            oracle_request["swap"] = json!({ "dex_id": "slimedragon.near/xyk", "message": "", "asset_in": "near", "constraint": near_fee });
        },
    );
    assert_eq!(
        quote_problems("near_to_usdc_exact_in", json),
        Vec::<String>::new()
    );
}

#[test]
fn a_fee_discount_on_intear_dex() {
    let json = tamper(
        quote_json("near_to_usdc_exact_in", "IntearDex"),
        0,
        0,
        |args, _| {
            args["operations"]["fee_discount"] = json!({
                "discount": {
                    "ForTrader": { "trader_id": "intear.near", "fee_discount": 250_000, "expiration_block_height": 190_000_000 }
                },
                "signature": "c2lnbmF0dXJl",
            });
        },
    );
    assert_eq!(
        quote_problems("near_to_usdc_exact_in", json),
        Vec::<String>::new()
    );
}

mod left_to_the_router {
    use super::*;

    const NEAR_TO_USDC: &str = "near_to_usdc_exact_in";

    #[track_caller]
    fn assert_safe(json: Value) {
        assert_eq!(quote_problems(NEAR_TO_USDC, json), Vec::<String>::new());
    }

    fn intear_dex() -> Value {
        quote_json(NEAR_TO_USDC, "IntearDex")
    }

    #[test]
    fn fields_of_intear_dex_that_routes_may_add() {
        assert_safe(tamper(intear_dex(), 0, 0, |args, _| {
            args["operations"]["new_field"] = "value".into();
            engine_operation(args, "SwapAdvanced")["new_field"] = "value".into();
            engine_operation(args, "OracleRequest")["request"] = json!({ "a new": "shape" });
        }));
    }

    /// Which only get what Intear DEX can't send to the trader
    #[test]
    fn rescue_addresses() {
        assert_safe(tamper(intear_dex(), 0, 0, |args, _| {
            engine_operation(args, "OracleRequest")["rescue_address"] = "wallet.intear.near".into();
            engine_operation(args, "Withdraw")["rescue_address"] = "wallet.intear.near".into();
        }));
    }

    #[test]
    fn withdrawing_to_the_trader() {
        assert_safe(tamper(intear_dex(), 0, 0, |args, _| {
            engine_operation(args, "Withdraw")["to"] = "intear.near".into();
        }));
        let json = tamper(
            quote_json("intear_balances_exact_in", "RheaDcl"),
            0,
            0,
            |args, _| {
                args["amount"] = json!({ "Full": { "at_least": null } });
                args["withdraw_to"] = "intear.near".into();
            },
        );
        assert_eq!(
            quote_problems("intear_balances_exact_in", json),
            Vec::<String>::new()
        );
    }

    #[test]
    fn operations_that_move_none_of_the_traders_assets() {
        assert_safe(tamper(intear_dex(), 0, 0, |args, _| {
            let operations = engine_operations(args);
            operations.insert(
                0,
                json!({ "RegisterAssets": { "asset_ids": ["near"], "for": null } }),
            );
            operations.insert(1, json!({ "NewOperation": {} }));
        }));
    }

    /// Which takes what Intear DEX can't send to the trader
    #[test]
    fn registering_the_referrer() {
        let mut json = intear_dex();
        json["execution_instructions"]
            .as_array_mut()
            .unwrap()
            .insert(
                0,
                transaction(
                    "dex.intear.near",
                    vec![one_yocto_call(
                        "register_assets",
                        json!({ "asset_ids": ["near"], "for": { "Account": "app.near" } }),
                    )],
                    false,
                ),
            );
        let request = SwapRequest {
            referrer_id: Some("app.near".parse().unwrap()),
            ..request_of(NEAR_TO_USDC)
        };
        assert_eq!(
            problems(check_quote(&request, &quote(json))),
            Vec::<String>::new()
        );
    }
}

mod tampered_quotes {
    use super::*;

    const NEAR_TO_USDC: &str = "near_to_usdc_exact_in";

    fn rhea() -> Value {
        quote_json(NEAR_TO_USDC, "Rhea")
    }

    fn rhea_dcl() -> Value {
        quote_json(NEAR_TO_USDC, "RheaDcl")
    }

    fn intear_dex() -> Value {
        quote_json(NEAR_TO_USDC, "IntearDex")
    }

    #[test]
    fn swapping_more_than_the_input() {
        let json = tamper(rhea(), 0, 1, |args, _| {
            args["amount"] = "1995000000000000000000000".into()
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "The transactions spend 2000000000000000000000000 of the input, more than the 1000000000000000000000000 the trader allows",
        );
    }

    #[test]
    fn swapping_the_whole_input_with_the_fees_on_top() {
        let json = tamper(rhea_dcl(), 0, 1, |args, _| {
            args["amount"] = ONE_NEAR.to_string().into()
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "spend 1005000000000000000000000 of the input, more than the 1000000000000000000000000",
        );
    }

    #[test]
    fn calling_another_contract() {
        let mut json = rhea();
        json["execution_instructions"][0]["NearTransaction"]["receiver_id"] = "evil.near".into();
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "evil.near near_deposit: Isn't one of the swap's tokens or the DEXes the router swaps on",
        );
    }

    #[test]
    fn sending_the_input_to_another_contract() {
        let json = tamper(rhea(), 0, 1, |args, _| {
            args["receiver_id"] = "evil.near".into()
        });
        assert_unsafe(NEAR_TO_USDC, json, "Sends the input to evil.near");
    }

    #[test]
    fn attaching_more_to_storage_deposits_than_the_quote_says() {
        let json = tamper(
            quote_json("rhea_balances_exact_in", "Rhea"),
            0,
            2,
            |_, call| {
                call["deposit"] = "50000000000000000000000".into();
            },
        );
        assert_unsafe(
            "rhea_balances_exact_in",
            json,
            "attach 74060000000000000000000 to storage deposits, more than the quote's 35260000000000000000000",
        );
    }

    #[test]
    fn registering_someone_the_swap_doesnt_pay() {
        let json = tamper(
            quote_json("rhea_balances_exact_in", "Rhea"),
            0,
            2,
            |args, _| {
                args["account_id"] = "evil.near".into();
            },
        );
        assert_unsafe("rhea_balances_exact_in", json, "Registers evil.near");
    }

    #[test]
    fn a_lower_minimum_output_on_rhea_dcl() {
        let json = tamper(rhea_dcl(), 0, 1, |args, _| {
            tamper_json(args, "msg", |msg| {
                msg["Swap"]["min_output_amount"] = "1".into()
            });
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "The swaps guarantee 1 of the output, less than the quote's worst case of 2817889",
        );
    }

    #[test]
    fn a_lower_minimum_output_on_rhea() {
        let json = tamper(rhea(), 0, 1, |args, _| {
            tamper_json(args, "msg", |msg| {
                let actions = msg["actions"].as_array_mut().unwrap();
                actions.last_mut().unwrap()["min_amount_out"] = "0".into();
            });
        });
        assert_unsafe(NEAR_TO_USDC, json, "The swaps guarantee 0 of the output");
    }

    #[test]
    fn an_argument_of_rhea_dcl_that_routes_dont_pass() {
        let json = tamper(rhea_dcl(), 0, 1, |args, _| {
            tamper_json(args, "msg", |msg| {
                msg["Swap"]["receiver_id"] = "evil.near".into()
            });
        });
        assert_unsafe(NEAR_TO_USDC, json, "unknown field `receiver_id`");
    }

    #[test]
    fn an_argument_of_rhea_that_routes_dont_pass() {
        let json = tamper(rhea(), 0, 1, |args, _| {
            tamper_json(args, "msg", |msg| {
                msg["swap_out_recipient"] = "evil.near".into()
            });
        });
        assert_unsafe(NEAR_TO_USDC, json, "unknown field `swap_out_recipient`");
    }

    #[test]
    fn an_argument_twice() {
        let mut json = rhea();
        let args = r#"{"receiver_id":"v2.ref-finance.near","amount":"1","amount":"995000000000000000000000","msg":""}"#;
        json["execution_instructions"][0]["NearTransaction"]["actions"][1]["FunctionCall"]["args"] =
            BASE64_STANDARD.encode(args).into();
        assert_unsafe(NEAR_TO_USDC, json, "duplicate field `amount`");
    }

    /// Which a contract with other arguments, or in another order, would read differently
    #[test]
    fn arguments_by_position() {
        let json = tamper(rhea_dcl(), 0, 1, |args, _| {
            *args = json!([args["receiver_id"], args["amount"], args["msg"]]);
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "invalid type: sequence, expected struct FtTransferCall",
        );

        let json = tamper(rhea_dcl(), 0, 1, |args, _| {
            tamper_json(args, "msg", |msg| {
                let swap = &msg["Swap"];
                msg["Swap"] = json!([
                    swap["pool_ids"],
                    swap["output_token"],
                    swap["min_output_amount"],
                    swap["skip_unwrap_near"],
                    "evil.near",
                ]);
            });
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "invalid type: sequence, expected struct RheaDclSwap",
        );

        let json = tamper(intear_dex(), 0, 0, |args, _| {
            let withdraw = engine_operation(args, "Withdraw");
            *withdraw = json!([withdraw["asset_id"], withdraw["amount"], null, null]);
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "invalid type: sequence, expected struct EngineWithdraw",
        );
    }

    #[test]
    fn a_fee_below_0_lowers_the_requested_fees() {
        let request = SwapRequest {
            fees: BTreeMap::from([
                ("wallet.intear.near".parse().unwrap(), 0.005),
                ("dex-aggregator.intear.near".parse().unwrap(), -0.004),
            ]),
            ..request_of(NEAR_TO_USDC)
        };
        let problems = problems(check_quote(&request, &quote(intear_dex())));
        assert!(
            problems.iter().any(|problem| problem
                .contains("Referral fees of 5000 millionths are more than the requested 1000")),
            "{problems:#?}"
        );
    }

    #[test]
    fn a_fee_to_someone_the_request_doesnt_pay() {
        let json = tamper(
            quote_json("rhea_balances_exact_in", "Rhea"),
            0,
            4,
            |args, _| {
                args["receiver_id"] = "evil.near".into();
            },
        );
        assert_unsafe(
            "rhea_balances_exact_in",
            json,
            "Pays a fee to evil.near, which the request doesn't",
        );
    }

    #[test]
    fn withdrawing_the_output_of_intear_dex_to_someone_else() {
        let json = tamper(intear_dex(), 0, 0, |args, _| {
            engine_operation(args, "Withdraw")["to"] = "evil.near".into();
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "Withdraws to someone else than the trader",
        );
    }

    #[test]
    fn operations_of_the_engine_that_move_the_traders_assets() {
        let transfer = json!({ "TransferAsset": {
            "to": { "Account": "evil.near" }, "asset_id": format!("nep141:{USDC}"), "amount": "1"
        } });
        let dex_call = json!({ "DexCall": {
            "dex_id": "evil.near/dex", "method": "drain", "args": "", "attached_assets": { "near": "1" }
        } });
        let storage =
            json!({ "StorageDeposit": { "amount": "1", "for": { "Account": "evil.near" } } });
        for (operation, kind) in [
            (transfer, "TransferAsset"),
            (dex_call, "DexCall"),
            (storage, "StorageDeposit"),
        ] {
            let json = tamper(intear_dex(), 0, 0, |args, _| {
                engine_operations(args).push(operation)
            });
            assert_unsafe(
                NEAR_TO_USDC,
                json,
                &format!("Moves the trader's assets with {kind}, which routes don't"),
            );
        }
    }

    #[test]
    fn swapping_the_entire_balance() {
        let json = tamper(intear_dex(), 0, 0, |args, _| {
            engine_operation(args, "SwapAdvanced")["amount"] = "EntireBalanceIn".into();
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "Swaps the entire balance, which routes don't",
        );
    }

    #[test]
    fn operations_as_a_bare_list() {
        let json = tamper(intear_dex(), 0, 0, |args, _| {
            args["operations"] = args["operations"]["operations"].clone();
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "invalid type: sequence, expected struct EngineOperations",
        );
    }

    #[test]
    fn withdrawing_a_balance_on_intear_dex_to_someone_else() {
        let json = tamper(
            quote_json("intear_balances_exact_in", "RheaDcl"),
            0,
            0,
            |args, _| {
                args["withdraw_to"] = "evil.near".into();
            },
        );
        assert_unsafe(
            "intear_balances_exact_in",
            json,
            "Withdraws to someone else than the trader",
        );
    }

    #[test]
    fn registering_assets_for_a_dex() {
        let json = tamper(
            quote_json("rhea_balances_exact_in", "IntearDex"),
            1,
            0,
            |args, _| {
                engine_operations(args).clear();
            },
        );
        let mut json = json;
        let register = one_yocto_call(
            "register_assets",
            json!({ "asset_ids": ["near"], "for": { "Dex": "evil.near/dex" } }),
        );
        json["execution_instructions"][1]["NearTransaction"]["actions"]
            .as_array_mut()
            .unwrap()
            .insert(0, register);
        assert_unsafe(
            "rhea_balances_exact_in",
            json,
            "Registers assets for the dex evil.near/dex, which routes don't",
        );
    }

    #[test]
    fn buying_the_oracle_fee_with_more_than_the_input() {
        let json = tamper(
            quote_json("intear_balances_exact_in", "IntearDex"),
            0,
            0,
            |args, _| {
                let operations = args["operations"].as_array_mut().unwrap();
                let oracle_request = operations
                    .iter_mut()
                    .find_map(|operation| operation.get_mut("OracleRequest"))
                    .unwrap();
                oracle_request["fee"] = json!([format!("nep141:{USDC}"), "1000"]);
                oracle_request["swap"] = json!({
                    "dex_id": "slimedragon.near/xyk",
                    "message": "",
                    "asset_in": "near",
                    "constraint": "1000000000000000000000000",
                });
            },
        );
        assert_unsafe(
            "intear_balances_exact_in",
            json,
            "spend 1999600000000000000000000 of the input, more than the 1000000000000000000000000",
        );
    }

    #[test]
    fn buying_the_oracle_fee_with_something_else_than_the_input() {
        let json = tamper(intear_dex(), 0, 0, |args, _| {
            engine_operation(args, "OracleRequest")["swap"] = json!({ "dex_id": "slimedragon.near/xyk", "message": "", "asset_in": format!("nep141:{USDC}"), "constraint": "1" });
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "Buys the oracle fee with something else than the input",
        );
    }

    #[test]
    fn buying_the_oracle_fee_without_a_limit() {
        let json = tamper(intear_dex(), 0, 0, |args, _| {
            engine_operation(args, "OracleRequest")["swap"] = json!({ "dex_id": "slimedragon.near/xyk", "message": "", "asset_in": "near", "constraint": null });
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "Buys the oracle fee with no limit of the input",
        );
    }

    #[test]
    fn paying_an_oracle_more_than_1_near() {
        let json = tamper(intear_dex(), 0, 0, |args, _| {
            engine_operation(args, "OracleRequest")["fee"] =
                json!(["near", "1000000000000000000000001"]);
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "Pays an oracle 1000000000000000000000001 of near, more than 1000000000000000000000000",
        );
    }

    #[test]
    fn buying_the_oracle_fee_with_more_than_1_near() {
        let json = tamper(intear_dex(), 0, 0, |args, _| {
            let oracle_request = engine_operation(args, "OracleRequest");
            oracle_request["fee"] = json!([format!("nep141:{USDC}"), "1000"]);
            oracle_request["swap"] = json!({
                "dex_id": "slimedragon.near/xyk",
                "message": "",
                "asset_in": "near",
                "constraint": "1000000000000000000000001",
            });
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "Pays an oracle 1000000000000000000000001 of near, more than 1000000000000000000000000",
        );
    }

    #[test]
    fn paying_an_oracle_with_an_asset_that_oracles_arent_paid_with() {
        let request = SwapRequest {
            token_in: "intear-dex:nep141:meme.near".parse().unwrap(),
            ..request_of("intear_balances_exact_in")
        };
        let json = tamper(
            quote_json("intear_balances_exact_in", "IntearDex"),
            0,
            0,
            |args, _| {
                let operations = args["operations"].as_array_mut().unwrap();
                let oracle_request = operations
                    .iter_mut()
                    .find_map(|operation| operation.get_mut("OracleRequest"))
                    .unwrap();
                oracle_request["fee"] = json!(["nep141:meme.near", "1"]);
            },
        );
        let problems = problems(check_quote(&request, &quote(json)));
        assert!(
            problems.iter().any(|problem| problem
                .contains("Pays an oracle with nep141:meme.near, which oracles aren't paid with")),
            "{problems:#?}"
        );
    }

    #[test]
    fn exact_out_on_intear_dex_without_a_swap() {
        let json = tamper(
            quote_json("near_to_usdc_exact_out", "IntearDex"),
            0,
            0,
            |args, _| {
                engine_operations(args).clear();
            },
        );
        assert_unsafe(
            "near_to_usdc_exact_out",
            json,
            "The swaps guarantee 0 of the output, less than the requested output of 2000000",
        );
    }

    #[test]
    fn referral_fees_on_intear_dex_on_top_of_the_fee_amount() {
        let fee = 5_000_000_000_000_000_000_000u128;
        let mut json = tamper(intear_dex(), 0, 0, |args, call| {
            let deposit: u128 = call["deposit"].as_str().unwrap().parse().unwrap();
            call["deposit"] = (deposit - fee).to_string().into();
            let amount = &mut engine_operation(args, "SwapAdvanced")["amount"]["Amount"]["ExactIn"];
            let exact_in: u128 = amount.as_str().unwrap().parse().unwrap();
            *amount = (exact_in - fee).to_string().into();
        });
        json["fee_amount"] = fee.to_string().into();
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "The fees of 9975000000000000000000 are more than the requested fees of at most 5000000000000000000002",
        );
    }

    #[test]
    fn a_quote_improvement_fee_of_what_the_estimate_already_has() {
        let json = tamper(intear_dex(), 0, 0, |args, _| {
            engine_operation(args, "SwapAdvanced")["quote_improvement_fee"]["quote_expected_amount"] =
                "2816571".into();
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "Takes a quote improvement fee of what beats 2816571",
        );
    }

    #[test]
    fn more_referral_fees_than_requested() {
        let json = tamper(intear_dex(), 0, 0, |args, _| {
            args["operations"]["referral_fees"] = json!([
                ["wallet.intear.near", 40_000],
                ["dex-aggregator.intear.near", 10_000]
            ]);
        });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "Referral fees of 50000 millionths are more than the requested 5000",
        );
    }

    #[test]
    fn a_worst_case_beyond_the_slippage() {
        let mut json = tamper(rhea_dcl(), 0, 1, |args, _| {
            tamper_json(args, "msg", |msg| {
                msg["Swap"]["min_output_amount"] = "2000000".into()
            });
        });
        json["worst_case_amount"] = json!({ "amount_out": "2000000" });
        assert_unsafe(
            NEAR_TO_USDC,
            json,
            "The worst case 2000000 is further from the estimate",
        );
    }

    #[test]
    fn the_most_gas_the_router_attaches_to_a_call() {
        let with_gas = |gas: u64| {
            let mut json = rhea_dcl();
            json["execution_instructions"][0]["NearTransaction"]["actions"][1]["FunctionCall"]["gas"] =
                gas.into();
            json
        };
        assert_eq!(
            quote_problems(NEAR_TO_USDC, with_gas(900_000_000_000_000)),
            Vec::<String>::new()
        );
        assert_unsafe(
            NEAR_TO_USDC,
            with_gas(900_000_000_000_001),
            "Attaches 900000000000001 gas, more than the router attaches to a call",
        );
    }

    #[test]
    fn more_transactions_than_the_router_finds_the_outcome_of() {
        let mut json = rhea_dcl();
        let unwrap = transaction(
            "wrap.near",
            vec![one_yocto_call("near_withdraw", json!({ "amount": "1" }))],
            false,
        );
        let instructions = json["execution_instructions"].as_array_mut().unwrap();
        instructions.extend(std::iter::repeat_n(unwrap, 20));
        assert_unsafe(NEAR_TO_USDC, json, "Has 21 transactions, more than 20");
    }

    #[test]
    fn swapping_exact_out_on_rhea() {
        let json = tamper(
            quote_json("near_to_usdc_exact_out", "RheaDcl"),
            0,
            1,
            |args, _| {
                args["receiver_id"] = "v2.ref-finance.near".into();
                args["msg"] = json!({
                    "force": 0,
                    "actions": [{
                        "pool_id": 1,
                        "token_in": "wrap.near",
                        "token_out": USDC,
                        "amount_in": args["amount"],
                        "amount_out": "0",
                        "min_amount_out": "0",
                    }],
                    "skip_degen_price_sync": true,
                    "skip_unwrap_near": true,
                    "referral_id": "wallet.intear.near",
                })
                .to_string()
                .into();
            },
        );
        assert_unsafe(
            "near_to_usdc_exact_out",
            json,
            "Swaps exact-out on Rhea, which routes don't",
        );
    }
}

mod shapes_of_quotes {
    use super::*;

    fn shape_error(json: Value) -> String {
        serde_json::from_value::<Quote>(json)
            .unwrap_err()
            .to_string()
    }

    fn rhea_dcl() -> Value {
        quote_json("near_to_usdc_exact_in", "RheaDcl")
    }

    #[test]
    fn transferring_near() {
        let mut json = rhea_dcl();
        let actions = json["execution_instructions"][0]["NearTransaction"]["actions"]
            .as_array_mut()
            .unwrap();
        actions.push(json!({ "Transfer": { "deposit": "1" } }));
        assert!(shape_error(json).contains("unknown variant `Transfer`"));
    }

    #[test]
    fn an_action_of_two_kinds() {
        let mut json = rhea_dcl();
        json["execution_instructions"][0]["NearTransaction"]["actions"][0]["Transfer"] =
            json!({ "deposit": "100000000000000000000000000" });
        assert!(shape_error(json).contains("expected map with a single key"));
    }

    #[test]
    fn a_deposit_that_isnt_a_decimal_amount() {
        let mut json = rhea_dcl();
        json["execution_instructions"][0]["NearTransaction"]["actions"][1]["FunctionCall"]["deposit"] =
            "0x1".into();
        assert!(shape_error(json).contains("invalid digit"));
    }

    #[test]
    fn gas_that_isnt_a_whole_amount() {
        let mut json = rhea_dcl();
        json["execution_instructions"][0]["NearTransaction"]["actions"][1]["FunctionCall"]["gas"] =
            1.5.into();
        assert!(shape_error(json).contains("invalid type: floating point `1.5`"));
    }

    #[test]
    fn a_quote_without_its_storage_deposits() {
        let mut json = rhea_dcl();
        json.as_object_mut().unwrap().remove("storage_deposits");
        assert!(shape_error(json).contains("missing field `storage_deposits`"));
    }

    #[test]
    fn a_quote_without_an_id() {
        let json = fixture_quote("near_to_usdc_exact_in", "RheaDcl");
        assert!(shape_error(json).contains("missing field `quote_id`"));
    }
}

mod fees_in_the_next_steps {
    use super::*;

    const NEAR_TO_USDC: &str = "near_to_usdc_exact_in";
    const ESTIMATED_OUT: u128 = 2_846_353;

    fn rhea_dcl() -> Quote {
        quote(quote_json(NEAR_TO_USDC, "RheaDcl"))
    }

    fn quote_improvement_fee(token: &str, receiver_id: &str, amount: u128) -> Value {
        transaction(
            token,
            vec![
                call(
                    "storage_deposit",
                    json!({ "registration_only": true, "account_id": receiver_id }),
                    "1250000000000000000000",
                    5_000_000_000_000,
                ),
                one_yocto_call(
                    "ft_transfer",
                    json!({ "receiver_id": receiver_id, "amount": amount.to_string(), "memo": "Quote improvement fee" }),
                ),
            ],
            true,
        )
    }

    /// Of a swap that beat the estimate by 1000: the trader spends 1 NEAR with the fees, and gets
    /// the estimate
    fn outcome(change: impl FnOnce(&mut Value)) -> SwapOutcome {
        let mut next_steps = fee_transactions(NEAR_TO_USDC, "RheaDcl");
        next_steps.push(quote_improvement_fee(USDC, "wallet.intear.near", 1000));
        let mut outcome = json!({
            "charge_fee": true,
            "amount_in": ONE_NEAR.to_string(),
            "amount_out": ESTIMATED_OUT.to_string(),
            "balance_changes": [],
            "next_steps": next_steps,
            "error": null,
            "settled": true,
            "quote_improvement_fee": "1000",
        });
        change(&mut outcome);
        serde_json::from_value(outcome).unwrap()
    }

    fn next_step_problems(quote: &Quote, outcome: &SwapOutcome) -> Vec<String> {
        problems(check_next_steps(&request_of(NEAR_TO_USDC), quote, outcome))
    }

    #[test]
    fn the_next_steps_pay_the_fees_and_the_quote_improvement_fee() {
        let next_steps =
            check_next_steps(&request_of(NEAR_TO_USDC), &rhea_dcl(), &outcome(|_| {})).unwrap();
        assert_eq!(next_steps.len(), 2);
    }

    #[test]
    fn a_quote_improvement_fee_of_more_than_the_swap_beat_the_quote_by() {
        let outcome =
            outcome(|outcome| outcome["amount_out"] = (ESTIMATED_OUT - 1).to_string().into());
        assert_eq!(
            next_step_problems(&rhea_dcl(), &outcome),
            [
                "With a quote improvement fee of 1000, the trader gets 2846352, not the quote's estimate of 2846353"
            ]
        );
    }

    #[test]
    fn a_quote_improvement_fee_of_less_than_the_swap_beat_the_quote_by() {
        let outcome =
            outcome(|outcome| outcome["amount_out"] = (ESTIMATED_OUT + 1).to_string().into());
        assert_eq!(
            next_step_problems(&rhea_dcl(), &outcome),
            [
                "With a quote improvement fee of 1000, the trader gets 2846354, not the quote's estimate of 2846353"
            ]
        );
    }

    #[test]
    fn a_quote_improvement_fee_other_than_the_outcomes() {
        let outcome = outcome(|outcome| outcome["quote_improvement_fee"] = "500".into());
        assert_eq!(
            next_step_problems(&rhea_dcl(), &outcome),
            ["The next steps pay a quote improvement fee of 1000, not the outcome's 500"]
        );
    }

    #[test]
    fn a_quote_improvement_fee_to_someone_else() {
        let outcome = outcome(|outcome| {
            let next_steps = outcome["next_steps"].as_array_mut().unwrap();
            *next_steps.last_mut().unwrap() = quote_improvement_fee(USDC, "evil.near", 1000);
        });
        let problems = next_step_problems(&rhea_dcl(), &outcome);
        assert!(problems.contains(&format!(
            "{USDC} ft_transfer: Pays the quote improvement fee to evil.near, not the requested receiver"
        )));
    }

    #[test]
    fn more_fees_than_the_quotes_fee_amount() {
        let outcome = outcome(|outcome| {
            let next_steps = outcome["next_steps"].as_array_mut().unwrap();
            next_steps.extend(fee_transactions(NEAR_TO_USDC, "RheaDcl"));
        });
        assert_eq!(
            next_step_problems(&rhea_dcl(), &outcome),
            [
                "The next steps pay 10000000000000000000000 of fees, more than the quote's 5000000000000000000000"
            ]
        );
    }

    #[test]
    fn fees_after_a_swap_that_failed() {
        let outcome = outcome(|outcome| {
            outcome["charge_fee"] = false.into();
            outcome["error"] = "Slippage".into();
            outcome
                .as_object_mut()
                .unwrap()
                .remove("quote_improvement_fee");
        });
        let problems = next_step_problems(&rhea_dcl(), &outcome);
        assert!(
            problems.contains(
                &"The next steps pay fees, which are only due after a swap that went through"
                    .to_string()
            )
        );
    }

    #[test]
    fn paying_the_quote_improvement_fee_out_of_the_balance_on_rhea() {
        let fixture = "rhea_balances_exact_in";
        let quote = quote(quote_json(fixture, "RheaDcl"));
        let deposit_output = transaction(
            USDC,
            vec![one_yocto_call(
                "ft_transfer_call",
                json!({ "receiver_id": "v2.ref-finance.near", "amount": (ESTIMATED_OUT + 1000).to_string(), "msg": "" }),
            )],
            false,
        );
        let withdraw_fee = transaction(
            "v2.ref-finance.near",
            vec![one_yocto_call(
                "withdraw",
                json!({ "token_id": USDC, "amount": "1000", "skip_unwrap_near": true }),
            )],
            true,
        );
        let mut next_steps = vec![deposit_output];
        next_steps.extend(fee_transactions(fixture, "RheaDcl"));
        next_steps.extend([
            withdraw_fee,
            quote_improvement_fee(USDC, "wallet.intear.near", 1000),
        ]);
        let outcome = serde_json::from_value(json!({
            "charge_fee": true,
            "amount_in": ONE_NEAR.to_string(),
            "amount_out": ESTIMATED_OUT.to_string(),
            "balance_changes": [],
            "next_steps": next_steps,
            "error": null,
            "settled": true,
            "quote_improvement_fee": "1000",
        }))
        .unwrap();
        assert_eq!(
            problems(check_next_steps(&request_of(fixture), &quote, &outcome)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_quote_improvement_fee_of_exact_out_in_the_input() {
        let fixture = "near_to_usdc_exact_out";
        let quote = quote(quote_json(fixture, "RheaDcl"));
        // The trader spends the estimate: what the swap took, its fees, and the quote improvement
        // fee
        let estimated_in: u128 = 702_653_299_353_018_149_569_161;
        let mut next_steps = fee_transactions(fixture, "RheaDcl");
        next_steps.push(quote_improvement_fee(
            "wrap.near",
            "wallet.intear.near",
            1000,
        ));
        let outcome = serde_json::from_value(json!({
            "charge_fee": true,
            "amount_in": estimated_in.to_string(),
            "amount_out": "2000000",
            "balance_changes": [],
            "next_steps": next_steps,
            "error": null,
            "settled": true,
            "quote_improvement_fee": "1000",
        }))
        .unwrap();
        assert_eq!(
            problems(check_next_steps(&request_of(fixture), &quote, &outcome)),
            Vec::<String>::new()
        );
    }
}

mod next_steps_of_a_failed_swap {
    use super::*;

    const NEAR_TO_USDC: &str = "near_to_usdc_exact_in";

    fn next_step_problems(next_steps: Vec<Value>) -> Vec<String> {
        let outcome = serde_json::from_value(json!({
            "charge_fee": false,
            "amount_in": "0",
            "amount_out": "0",
            "balance_changes": [],
            "next_steps": next_steps,
            "error": "Slippage",
            "settled": true,
        }))
        .unwrap();
        let quote = quote(quote_json(NEAR_TO_USDC, "RheaDcl"));
        problems(check_next_steps(
            &request_of(NEAR_TO_USDC),
            &quote,
            &outcome,
        ))
    }

    #[test]
    fn returning_the_wnear_of_a_failed_swap() {
        let unwrap = transaction(
            "wrap.near",
            vec![one_yocto_call(
                "near_withdraw",
                json!({ "amount": "995000000000000000000000" }),
            )],
            false,
        );
        assert_eq!(next_step_problems(vec![unwrap]), Vec::<String>::new());
    }

    #[test]
    fn transferring_the_traders_tokens_away() {
        let transfer = transaction(
            "wrap.near",
            vec![one_yocto_call(
                "ft_transfer",
                json!({ "receiver_id": "evil.near", "amount": "995000000000000000000000", "memo": "Trading fee (evil.near)" }),
            )],
            false,
        );
        assert_eq!(
            next_step_problems(vec![transfer]),
            ["wrap.near ft_transfer: Transfers tokens to evil.near outside of a fee transaction"]
        );
    }

    #[test]
    fn swapping() {
        let swap = quote_json(NEAR_TO_USDC, "RheaDcl")["execution_instructions"][0].clone();
        assert!(
            next_step_problems(vec![swap])
                .contains(&"wrap.near ft_transfer_call: Next steps don't swap".to_string())
        );
    }
}

mod limits_of_storage_deposits {
    use super::*;

    const RHEA_BALANCES: &str = "rhea_balances_exact_in";

    fn allowing_storage(mut json: Value) -> Value {
        json["storage_deposits"] = "100000000000000000000000000".into();
        json
    }

    fn rhea_storage(args: Value, deposit: &str) -> Value {
        let mut args = args;
        args["registration_only"] = false.into();
        call("storage_deposit", args, deposit, 3_000_000_000_000)
    }

    #[test]
    fn leaving_more_storage_on_rhea_to_someone_else_than_they_need() {
        let json = tamper(quote_json(RHEA_BALANCES, "Rhea"), 0, 2, |_, call| {
            call["deposit"] = "60000000000000000000000".into();
        });
        assert_unsafe(
            RHEA_BALANCES,
            allowing_storage(json),
            "Leaves 60000000000000000000000 of storage on Rhea to wallet.intear.near to withdraw, more than 50000000000000000000000",
        );
    }

    #[test]
    fn leaving_storage_on_rhea_to_someone_else_over_several_deposits() {
        let mut json = allowing_storage(quote_json(RHEA_BALANCES, "Rhea"));
        let deposit = rhea_storage(
            json!({ "account_id": "dex-aggregator.intear.near" }),
            "20000000000000000000000",
        );
        let actions = json["execution_instructions"][0]["NearTransaction"]["actions"]
            .as_array_mut()
            .unwrap();
        actions.extend([deposit.clone(), deposit]);
        assert_unsafe(
            RHEA_BALANCES,
            json,
            "Leaves 64060000000000000000000 of storage on Rhea to dex-aggregator.intear.near",
        );
    }

    #[test]
    fn attaching_more_than_the_limit_to_storage_deposits_in_all() {
        let mut json = allowing_storage(quote_json("near_to_usdc_exact_in", "RheaDcl"));
        let storage = transaction(
            "dclv2.ref-labs.near",
            vec![call(
                "storage_deposit",
                json!({ "registration_only": true }),
                "2000000000000000000000000",
                3_000_000_000_000,
            )],
            false,
        );
        json["execution_instructions"]
            .as_array_mut()
            .unwrap()
            .insert(0, storage);
        assert_unsafe(
            "near_to_usdc_exact_in",
            json,
            "attach 2000000000000000000000000 to storage deposits, more than 1000000000000000000000000 in all",
        );
    }

    #[test]
    fn of_the_next_steps() {
        let next_step_problems = |actions: Vec<Value>| {
            let outcome = serde_json::from_value(json!({
                "charge_fee": true,
                "amount_in": "995000000000000000000000",
                "amount_out": "2846353",
                "balance_changes": [],
                "next_steps": [transaction("v2.ref-finance.near", actions, false)],
                "error": null,
                "settled": true,
            }))
            .unwrap();
            let quote = quote(quote_json("near_to_usdc_exact_in", "RheaDcl"));
            problems(check_next_steps(
                &request_of("near_to_usdc_exact_in"),
                &quote,
                &outcome,
            ))
        };
        let to_others = vec![
            rhea_storage(
                json!({ "account_id": "dex-aggregator.intear.near" }),
                "30000000000000000000000"
            );
            2
        ];
        assert_eq!(
            next_step_problems(to_others),
            [
                "v2.ref-finance.near storage_deposit: Leaves 60000000000000000000000 of storage on Rhea to dex-aggregator.intear.near to withdraw, more than 50000000000000000000000"
            ]
        );
        let in_all = vec![rhea_storage(json!({}), "100000000000000000000000"); 11];
        assert_eq!(
            next_step_problems(in_all),
            [
                "The transactions attach 1100000000000000000000000 to storage deposits, more than 1000000000000000000000000 in all"
            ]
        );
    }
}

mod chains_of_swaps_on_intear_dex {
    use super::*;

    /// The quote's swap as two through USDT, the way the router chains them
    fn chained(fixture: &str, change: impl FnOnce(&mut Value, &mut Value)) -> Value {
        tamper(quote_json(fixture, "IntearDex"), 0, 0, |args, _| {
            let operations = engine_operations(args);
            let index = operations
                .iter()
                .position(|operation| {
                    operation
                        .get("SwapSimple")
                        .or(operation.get("SwapAdvanced"))
                        .is_some()
                })
                .unwrap();
            let swap = operations[index]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .clone();
            let mut first = json!({
                "dex_id": "slimedragon.near/xyk",
                "message": "",
                "asset_in": swap["asset_in"],
                "asset_out": USDT,
                "amount": swap["amount"],
                "constraint": null,
                "quote_improvement_fee": null,
            });
            let mut second = json!({
                "dex_id": "slimedragon.near/xyk",
                "message": "",
                "asset_in": USDT,
                "asset_out": swap["asset_out"],
                "amount": "OutputOfLastIn",
                "constraint": swap["constraint"],
                "quote_improvement_fee": swap["quote_improvement_fee"],
            });
            change(&mut first, &mut second);
            operations.splice(
                index..=index,
                [
                    json!({ "SwapSimple": first }),
                    json!({ "SwapSimple": second }),
                ],
            );
        })
    }

    #[test]
    fn swapping_through_another_token() {
        let json = chained("near_to_usdc_exact_in", |_, _| {});
        assert_eq!(
            quote_problems("near_to_usdc_exact_in", json),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_lower_minimum_output_at_the_end() {
        let json = chained("near_to_usdc_exact_in", |_, second| {
            second["constraint"] = "1".into()
        });
        assert_unsafe("near_to_usdc_exact_in", json, "guarantee 1 of the output");
    }

    #[test]
    fn constraining_a_swap_before_the_last_one() {
        let json = chained("near_to_usdc_exact_in", |first, _| {
            first["constraint"] = "1".into()
        });
        assert_unsafe(
            "near_to_usdc_exact_in",
            json,
            "Constrains a swap before the last one of a chain",
        );
    }

    #[test]
    fn swapping_something_else_than_the_swap_before_gave() {
        let json = chained("near_to_usdc_exact_in", |_, second| {
            second["asset_in"] = "near".into()
        });
        assert_unsafe(
            "near_to_usdc_exact_in",
            json,
            "Swaps something else than what the swap before it gave",
        );
    }

    #[test]
    fn swapping_what_a_withdrawal_gave() {
        let json = tamper(
            chained("near_to_usdc_exact_in", |first, second| {
                // The first swap gives the output on its own, then the second swaps it again
                first["asset_out"] = second["asset_out"].clone();
                first["constraint"] = second["constraint"].clone();
                second["asset_in"] = second["asset_out"].clone();
            }),
            0,
            0,
            |args, _| {
                let operations = engine_operations(args);
                let second = operations
                    .iter()
                    .rposition(|operation| operation.get("SwapSimple").is_some())
                    .unwrap();
                let asset_id = operations[second]["SwapSimple"]["asset_in"].clone();
                let withdraw = json!({
                    "Withdraw": { "asset_id": asset_id, "amount": { "Full": { "at_least": null } }, "to": null, "rescue_address": null }
                });
                operations.insert(second, withdraw);
            },
        );
        assert_unsafe(
            "near_to_usdc_exact_in",
            json,
            "Swaps something else than what the swap before it gave",
        );
    }

    #[test]
    fn ending_in_something_else_than_the_output() {
        let json = chained("near_to_usdc_exact_in", |_, second| {
            second["asset_out"] = USDT.into()
        });
        assert_unsafe(
            "near_to_usdc_exact_in",
            json,
            "Swaps to something else than the output",
        );
    }

    #[test]
    fn chaining_exact_out_swaps() {
        let json = chained("near_to_usdc_exact_out", |_, _| {});
        assert_unsafe("near_to_usdc_exact_out", json, "only exact-in swaps do");
    }
}

mod split_swaps_on_intear_dex {
    use super::*;

    /// The quote's swap split into two of its own parts of the input, the way the router splits it
    fn split(change: impl FnOnce(&mut Value)) -> Value {
        tamper(
            quote_json("near_to_usdc_exact_in", "IntearDex"),
            0,
            0,
            |args, _| {
                let operations = engine_operations(args);
                let index = operations
                    .iter()
                    .position(|operation| {
                        operation
                            .get("SwapSimple")
                            .or(operation.get("SwapAdvanced"))
                            .is_some()
                    })
                    .unwrap();
                let swap = operations[index]
                    .as_object()
                    .unwrap()
                    .values()
                    .next()
                    .unwrap()
                    .clone();
                let number = |value: &Value| value.as_str().unwrap().parse::<u128>().unwrap();
                let amount = number(&swap["amount"]["Amount"]["ExactIn"]);
                let constraint = number(&swap["constraint"]);
                let expected = number(&swap["quote_improvement_fee"]["quote_expected_amount"]);
                let part = |share: &dyn Fn(u128) -> u128| {
                    let mut fee = swap["quote_improvement_fee"].clone();
                    fee["quote_expected_amount"] = share(expected).to_string().into();
                    json!({
                        "dex_id": "slimedragon.near/xyk",
                        "message": "",
                        "asset_in": swap["asset_in"],
                        "asset_out": swap["asset_out"],
                        "amount": { "Amount": { "ExactIn": share(amount).to_string() } },
                        "constraint": share(constraint).to_string(),
                        "quote_improvement_fee": fee,
                    })
                };
                let first = part(&|whole| whole * 7 / 10);
                let mut second = part(&|whole| whole - whole * 7 / 10);
                change(&mut second);
                operations.splice(
                    index..=index,
                    [
                        json!({ "SwapSimple": first }),
                        json!({ "SwapSimple": second }),
                    ],
                );
            },
        )
    }

    fn one_less(value: &mut Value) {
        let amount = value.as_str().unwrap().parse::<u128>().unwrap();
        *value = (amount - 1).to_string().into();
    }

    #[test]
    fn a_split_swap() {
        assert_eq!(
            quote_problems("near_to_usdc_exact_in", split(|_| {})),
            Vec::<String>::new()
        );
    }

    #[test]
    fn quote_improvement_fees_of_what_the_estimate_already_has_between_them() {
        let json =
            split(|second| one_less(&mut second["quote_improvement_fee"]["quote_expected_amount"]));
        assert_unsafe(
            "near_to_usdc_exact_in",
            json,
            "Takes a quote improvement fee of what beats",
        );
    }

    #[test]
    fn guaranteeing_less_than_the_worst_case_between_them() {
        let json = split(|second| one_less(&mut second["constraint"]));
        assert_unsafe("near_to_usdc_exact_in", json, "The swaps guarantee");
    }
}

#[test]
fn route_query() {
    let request = SwapRequest {
        dexes: Some(vec!["Rhea".to_string(), "IntearDex".to_string()]),
        ..request_of("rhea_balances_exact_in")
    };
    assert_eq!(
        request.route_query(),
        format!(
            "token_in=rhea-nep141%3Awrap.near&token_out=rhea-nep141%3A{USDC}&amount_in=1000000000000000000000000\
            &slippage_type=Fixed&slippage=0.01&trader_account_id=intear.near&dexes=Rhea%2CIntearDex\
            &referrer_id=wallet.intear.near&fees=%7B%22wallet.intear.near%22%3A0.005%7D\
            &quote_improvement_receiver=wallet.intear.near"
        )
    );
}

#[test]
fn token_ids() {
    for token in [
        "near",
        "nep141:usdt.tether-token.near",
        "rhea-nep141:wrap.near",
        "intear-dex:near",
        "intear-dex:nep141:wrap.near",
    ] {
        assert_eq!(token.parse::<crate::TokenId>().unwrap().to_string(), token);
    }
    assert_eq!(
        "usdt.tether-token.near"
            .parse::<crate::TokenId>()
            .unwrap()
            .to_string(),
        "nep141:usdt.tether-token.near"
    );
    assert_eq!(
        "intear-dex:nep171:nft.near:1"
            .parse::<crate::TokenId>()
            .unwrap_err()
            .to_string(),
        "intear-dex:nep171:nft.near:1 isn't a token the router swaps: Intear DEX only has NEAR and NEP-141s"
    );
}
