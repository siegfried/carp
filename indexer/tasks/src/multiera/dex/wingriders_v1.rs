use std::collections::BTreeMap;

use pallas::ledger::{
    primitives::ToCanonicalJson,
    traverse::{MultiEraOutput, MultiEraTx},
};

use super::common::{
    build_asset, filter_outputs_and_datums_by_hash, reduce_ada_amount, Dex, DexType,
    QueuedMeanPrice, QueuedSwap, WingRidersV1,
};
use crate::{
    era_common::OutputWithTxData,
    multiera::utils::common::{get_asset_amount, get_plutus_datum_for_output},
};
use entity::dex_swap::Operation;

const POOL_SCRIPT_HASH: &str = "e6c90a5923713af5786963dee0fdffd830ca7e0c86a041d9e5833e91";
const POOL_FIXED_ADA: u64 = 3_000_000; // every pool UTXO holds this amount of ADA
const SWAP_IN_ADA: u64 = 4_000_000; // oil ADA + agent fee
const SWAP_OUT_ADA: u64 = 2_000_000; // oil ADA

impl Dex for WingRidersV1 {
    fn queue_mean_price(
        &self,
        queued_prices: &mut Vec<QueuedMeanPrice>,
        tx: &MultiEraTx,
        tx_id: i64,
    ) -> Result<(), String> {
        // Note: there should be at most one pool output
        if let Some((output, datum)) =
            filter_outputs_and_datums_by_hash(&tx.outputs(), &[POOL_SCRIPT_HASH], &tx.plutus_data())
                .get(0)
        {
            let datum = datum.to_json();

            let treasury1 = datum["fields"][1]["fields"][2]["int"]
                .as_u64()
                .ok_or("Failed to parse treasury1")?;
            let treasury2 = datum["fields"][1]["fields"][3]["int"]
                .as_u64()
                .ok_or("Failed to parse treasury2")?;

            let parse_asset_item = |i, j| -> Result<Vec<u8>, &str> {
                let item = datum["fields"][1]["fields"][0]["fields"][i]["fields"][j]["bytes"]
                    .as_str()
                    .ok_or("Failed to parse asset item")?
                    .to_string();
                hex::decode(item).map_err(|_e| "Failed to parse asset item")
            };
            let asset1 = build_asset(parse_asset_item(0, 0)?, parse_asset_item(0, 1)?);
            let asset2 = build_asset(parse_asset_item(1, 0)?, parse_asset_item(1, 1)?);

            let amount1 = get_asset_amount(output, &asset1)
                - treasury1
                - reduce_ada_amount(&asset1, POOL_FIXED_ADA);
            let amount2 = get_asset_amount(output, &asset2)
                - treasury2
                - reduce_ada_amount(&asset2, POOL_FIXED_ADA);

            queued_prices.push(QueuedMeanPrice {
                tx_id,
                address: output.address().unwrap().to_vec(),
                dex_type: DexType::WingRidersV1,
                asset1,
                asset2,
                amount1,
                amount2,
            });
        }
        Ok(())
    }

    fn queue_swap(
        &self,
        queued_swaps: &mut Vec<QueuedSwap>,
        tx: &MultiEraTx,
        tx_id: i64,
        multiera_used_inputs_to_outputs_map: &BTreeMap<Vec<u8>, BTreeMap<i64, OutputWithTxData>>,
    ) -> Result<(), String> {
        // Note: there should be at most one pool output
        if let Some((pool_output, _)) =
            filter_outputs_and_datums_by_hash(&tx.outputs(), &[POOL_SCRIPT_HASH], &tx.plutus_data())
                .get(0)
        {
            let redeemers = tx.redeemers().ok_or("No redeemers")?;

            // Get pool input from redemeers
            let pool_input_redeemer = redeemers.get(0).ok_or("No redeemers")?;
            let pool_input = pool_input_redeemer.data.to_json()["fields"][0]["int"]
                .as_i64()
                .ok_or("Failed to parse pool input index")?;

            // Find main redemeer
            let redeemer = redeemers
                .iter()
                .find(|&r| r.index as i64 == pool_input)
                .ok_or("Failed to find main redeemer")?;
            let redeemer = redeemer.data.to_json();

            // Extract input list from redemeer
            let redeemer_map: Vec<usize> = redeemer["fields"][2]["list"]
                .as_array()
                .ok_or("Failed to parse redeemer map")?
                .iter()
                .map(|r| r["int"].as_i64().unwrap() as usize)
                .collect();
            // Find main transaction
            let parent = redeemer["fields"][0]["int"]
                .as_i64()
                .ok_or("Failed to parse main transaction")? as usize;
            // Restore inputs
            let inputs: Vec<MultiEraOutput> = tx
                .inputs()
                .iter()
                .map(|i| {
                    let output = &multiera_used_inputs_to_outputs_map[&i.hash().to_vec()]
                        [&(i.index() as i64)];
                    MultiEraOutput::decode(output.era, &output.model.payload).unwrap()
                })
                .collect::<Vec<_>>();
            // Zip outputs with redemeer index
            for (output, redeemer) in tx.outputs().iter().skip(1).zip(redeemer_map) {
                // pair input with output
                let input = inputs.get(redeemer).ok_or("Failed to pair output")?.clone();

                // get information about swap from pool plutus data
                let parent_datum = get_plutus_datum_for_output(&inputs[parent], &tx.plutus_data())
                    .unwrap()
                    .to_json();
                let parse_asset_item = |i, j| -> Result<Vec<u8>, &str> {
                    let item = parent_datum["fields"][1]["fields"][0]["fields"][i]["fields"][j]
                        ["bytes"]
                        .as_str()
                        .ok_or("Failed to parse asset item")?
                        .to_string();
                    hex::decode(item).map_err(|_e| "Failed to parse asset item")
                };
                let asset1 = build_asset(parse_asset_item(0, 0)?, parse_asset_item(0, 1)?);
                let asset2 = build_asset(parse_asset_item(1, 0)?, parse_asset_item(1, 1)?);

                // get actual plutus datum
                let input_datum = get_plutus_datum_for_output(&input, &tx.plutus_data())
                    .unwrap()
                    .to_json();
                // identify operation: 0 = swap
                let operation = input_datum["fields"][1]["constructor"]
                    .as_i64()
                    .ok_or("Failed to parse operation")?;
                if operation != 0 {
                    tracing::debug!("Operation is not a swap");
                    continue;
                }
                let direction = input_datum["fields"][1]["fields"][0]["constructor"]
                    .as_i64()
                    .ok_or("Failed to parse direction")?;

                let amount1;
                let amount2;
                if direction == 0 {
                    amount1 =
                        get_asset_amount(&input, &asset1) - reduce_ada_amount(&asset1, SWAP_IN_ADA);
                    amount2 = get_asset_amount(output, &asset2)
                        - reduce_ada_amount(&asset2, SWAP_OUT_ADA);
                } else {
                    amount1 = get_asset_amount(output, &asset1)
                        - reduce_ada_amount(&asset1, SWAP_OUT_ADA);
                    amount2 =
                        get_asset_amount(&input, &asset2) - reduce_ada_amount(&asset2, SWAP_IN_ADA);
                }
                queued_swaps.push(QueuedSwap {
                    tx_id,
                    address: pool_output.address().unwrap().to_vec(),
                    dex_type: DexType::WingRidersV1,
                    asset1,
                    asset2,
                    amount1,
                    amount2,
                    operation: match direction == 0 {
                        true => Operation::Sell,
                        false => Operation::Buy,
                    },
                })
            }
        }
        Ok(())
    }
}
