//! Tests that orders that provide just-in-time liquidity get correctly
//! serialized.

use {
    crate::tests::{self, legacy},
    serde_json::json,
};

#[tokio::test]
async fn test() {
    let legacy_solver = tests::legacy::setup(vec![legacy::Expectation {
        req: json!({
            "amms": {},
            "metadata": {
                "auction_id": null,
                "environment": null,
                "gas_price": 15000000000.0,
                "native_token": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "run_id": null
            },
            "orders": {},
            "tokens": {}
        }),
        res: json!({
            "orders": {},
            "prices": {},
            "amms": {},
            "foreign_liquidity_orders": [
                {
                    "order": {
                        "from": "0x1111111111111111111111111111111111111111",
                        "sellToken": "0x2222222222222222222222222222222222222222",
                        "buyToken": "0x3333333333333333333333333333333333333333",
                        "receiver": "0x4444444444444444444444444444444444444444",
                        "sellAmount": "100",
                        "buyAmount": "200",
                        "validTo": 1000,
                        "appData": "0x6000000000000000000000000000000000000000000000000000000000000007",
                        "feeAmount": "321",
                        "kind": "sell",
                        "partiallyFillable": true,
                        "sellTokenBalance": "erc20",
                        "buyTokenBalance": "erc20",
                        "signingScheme": "eip712",
                        "signature": "0x\
                            0101010101010101010101010101010101010101010101010101010101010101\
                            0202020202020202020202020202020202020202020202020202020202020202\
                            03",
                        "interactions": {
                            "pre": [
                                {
                                    "target": "0x2222222222222222222222222222222222222222",
                                    "value": "200",
                                    "callData": "0xabcd",
                                }
                            ]
                        }
                    },
                    "exec_sell_amount": "100",
                    "exec_buy_amount": "200",
                }
            ],
        }),
    }])
    .await;
    let config = legacy::create_temp_config_file(&legacy_solver);

    let engine =
        tests::SolverEngine::new("legacy".to_owned(), config.to_str().unwrap().to_owned()).await;

    let solution = engine
        .solve(json!({
            "id": null,
            "tokens": {},
            "orders": [],
            "liquidity": [],
            "effectiveGasPrice": "15000000000",
            "deadline": "2106-01-01T00:00:00.000Z"
        }))
        .await;

    assert_eq!(
        solution,
        json!({
            "prices": {},
            "trades": [
                {
                    "kind": "jit",
                    "order": {
                        "sellToken": "0x2222222222222222222222222222222222222222",
                        "buyToken": "0x3333333333333333333333333333333333333333",
                        "receiver": "0x4444444444444444444444444444444444444444",
                        "sellAmount": "100",
                        "buyAmount": "200",
                        "validTo": 1000,
                        "appData": "0x6000000000000000000000000000000000000000000000000000000000000007",
                        "feeAmount": "321",
                        "kind": "sell",
                        "partiallyFillable": true,
                        "sellTokenBalance": "erc20",
                        "buyTokenBalance": "erc20",
                        "signingScheme": "eip712",
                        "signature": "0x\
                            0101010101010101010101010101010101010101010101010101010101010101\
                            0202020202020202020202020202020202020202020202020202020202020202\
                            03",
                        "preInteractions": [
                            {
                                "target": "0x2222222222222222222222222222222222222222",
                                "value": "200",
                                "calldata": "0xabcd",
                            }
                        ]
                    },
                    "executedAmount": "100",
                }
            ],
            "interactions": [],
        }),
    );
}