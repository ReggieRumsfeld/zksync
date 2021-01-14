//! Module used to calculate fee for transactions.
//!
//! base formula for calculation:
//! `( zkp cost of chunk * number of chunks + gas price of transaction) * token risk factor / cost of token is usd`

// Built-in deps
use std::collections::{HashMap, HashSet};
// External deps
use bigdecimal::BigDecimal;
use futures::{
    channel::{mpsc::Receiver, oneshot},
    StreamExt,
};
use num::{
    rational::Ratio,
    traits::{Inv, Pow},
    BigUint,
};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio::time::Instant;
// Workspace deps
use zksync_config::{FeeTickerOptions, TokenPriceSource};
use zksync_storage::ConnectionPool;
use zksync_types::{
    Address, ChangePubKeyOp, Token, TokenId, TokenLike, TransferToNewOp, TxFeeTypes, WithdrawOp,
};

use zksync_utils::ratio_to_big_decimal;
// Local deps
use crate::fee_ticker::{
    ticker_api::{
        coingecko::CoinGeckoAPI, coinmarkercap::CoinMarketCapAPI, FeeTickerAPI, TickerApi,
        CONNECTION_TIMEOUT,
    },
    validator::{
        watcher::{TokenWatcher, UniswapTokenWatcher},
        FeeTokenValidator,
    },
};
use crate::utils::token_db_cache::TokenDBCache;

pub use self::fee::*;
use crate::fee_ticker::balancer::TickerBalancer;
use crate::fee_ticker::validator::MarketUpdater;
use std::convert::TryFrom;

mod constants;
mod fee;
mod ticker_api;
pub mod validator;

mod balancer;
#[cfg(test)]
mod tests;

/// Contains cost of zkSync operations in Wei.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GasOperationsCost {
    standard_cost: HashMap<OutputFeeType, BigUint>,
    subsidize_cost: HashMap<OutputFeeType, BigUint>,
}

impl GasOperationsCost {
    pub fn from_constants(fast_processing_coeff: f64) -> Self {
        // We increase gas price for fast withdrawals, since it will induce generating a smaller block
        // size, resulting in us paying more gas than for bigger block.
        let standard_fast_withdrawal_cost =
            (constants::BASE_WITHDRAW_COST as f64 * fast_processing_coeff) as u32;
        let subsidy_fast_withdrawal_cost =
            (constants::SUBSIDY_WITHDRAW_COST as f64 * fast_processing_coeff) as u32;

        let standard_cost = vec![
            (
                OutputFeeType::TransferToNew,
                constants::BASE_TRANSFER_TO_NEW_COST.into(),
            ),
            (
                OutputFeeType::Withdraw,
                constants::BASE_WITHDRAW_COST.into(),
            ),
            (
                OutputFeeType::FastWithdraw,
                standard_fast_withdrawal_cost.into(),
            ),
            (
                OutputFeeType::ChangePubKey {
                    onchain_pubkey_auth: false,
                },
                constants::BASE_CHANGE_PUBKEY_OFFCHAIN_COST.into(),
            ),
            (
                OutputFeeType::ChangePubKey {
                    onchain_pubkey_auth: true,
                },
                constants::BASE_CHANGE_PUBKEY_ONCHAIN_COST.into(),
            ),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();

        let subsidize_cost = vec![
            (
                OutputFeeType::TransferToNew,
                constants::SUBSIDY_TRANSFER_TO_NEW_COST.into(),
            ),
            (
                OutputFeeType::Withdraw,
                constants::SUBSIDY_WITHDRAW_COST.into(),
            ),
            (
                OutputFeeType::FastWithdraw,
                subsidy_fast_withdrawal_cost.into(),
            ),
            (
                OutputFeeType::ChangePubKey {
                    onchain_pubkey_auth: false,
                },
                constants::SUBSIDY_CHANGE_PUBKEY_OFFCHAIN_COST.into(),
            ),
            (
                OutputFeeType::ChangePubKey {
                    onchain_pubkey_auth: true,
                },
                constants::BASE_CHANGE_PUBKEY_ONCHAIN_COST.into(),
            ),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();

        Self {
            standard_cost,
            subsidize_cost,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TickerConfig {
    zkp_cost_chunk_usd: Ratio<BigUint>,
    gas_cost_tx: GasOperationsCost,
    tokens_risk_factors: HashMap<TokenId, Ratio<BigUint>>,
    not_subsidized_tokens: HashSet<Address>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TokenPriceRequestType {
    USDForOneWei,
    USDForOneToken,
}

#[derive(Debug)]
pub enum TickerRequest {
    GetTxFee {
        tx_type: TxFeeTypes,
        token: TokenLike,
        response: oneshot::Sender<Result<Fee, anyhow::Error>>,
    },
    GetTokenPrice {
        token: TokenLike,
        response: oneshot::Sender<Result<BigDecimal, anyhow::Error>>,
        req_type: TokenPriceRequestType,
    },
    IsTokenAllowed {
        token: TokenLike,
        response: oneshot::Sender<Result<bool, anyhow::Error>>,
    },
}

struct FeeTicker<API, WATCHER> {
    api: API,
    requests: Receiver<TickerRequest>,
    config: TickerConfig,
    validator: FeeTokenValidator<WATCHER>,
}

#[must_use]
pub fn run_ticker_task(
    db_pool: ConnectionPool,
    tricker_requests: Receiver<TickerRequest>,
) -> JoinHandle<()> {
    let config = FeeTickerOptions::from_env();

    let ticker_config = TickerConfig {
        zkp_cost_chunk_usd: Ratio::from_integer(BigUint::from(10u32).pow(3u32)).inv(),
        gas_cost_tx: GasOperationsCost::from_constants(config.fast_processing_coeff),
        tokens_risk_factors: HashMap::new(),
        not_subsidized_tokens: config.not_subsidized_tokens,
    };

    let cache = (db_pool.clone(), TokenDBCache::new());
    let watcher = UniswapTokenWatcher::new(config.uniswap_url);
    let validator = FeeTokenValidator::new(
        cache.clone(),
        chrono::Duration::seconds(config.available_liquidity_seconds as i64),
        BigDecimal::try_from(config.liquidity_volume).expect("Valid f64 for decimal"),
        config.unconditionally_valid_tokens,
        watcher.clone(),
    );

    let updater = MarketUpdater::new(cache, watcher);
    tokio::spawn(updater.keep_updated(config.token_market_update_time));
    let client = reqwest::ClientBuilder::new()
        .timeout(CONNECTION_TIMEOUT)
        .connect_timeout(CONNECTION_TIMEOUT)
        .build()
        .expect("Failed to build reqwest::Client");
    match config.token_price_source {
        TokenPriceSource::CoinMarketCap { base_url } => {
            let token_price_api = CoinMarketCapAPI::new(client, base_url);

            let ticker_api = TickerApi::new(db_pool.clone(), token_price_api);
            let fee_ticker = FeeTicker::new(ticker_api, tricker_requests, ticker_config, validator);

            tokio::spawn(fee_ticker.run())
        }

        TokenPriceSource::CoinGecko { base_url } => {
            let token_price_api =
                CoinGeckoAPI::new(client, base_url).expect("CoinGecko initializing error");

            let mut ticker_balancer = TickerBalancer::new(
                token_price_api,
                ticker_config,
                validator,
                tricker_requests,
                db_pool,
                config.number_of_ticker_actors,
            );
            ticker_balancer.spawn_tickers();
            tokio::spawn(ticker_balancer.run())
        }
    }
}

impl<API: FeeTickerAPI, WATCHER: TokenWatcher> FeeTicker<API, WATCHER> {
    fn new(
        api: API,
        requests: Receiver<TickerRequest>,
        config: TickerConfig,
        validator: FeeTokenValidator<WATCHER>,
    ) -> Self {
        Self {
            api,
            requests,
            config,
            validator,
        }
    }

    async fn run(mut self) {
        while let Some(request) = self.requests.next().await {
            let start = Instant::now();
            match request {
                TickerRequest::GetTxFee {
                    tx_type,
                    token,
                    response,
                } => {
                    let fee = self.get_fee_from_ticker_in_wei(tx_type, token).await;
                    metrics::histogram!("ticker.get_tx_fee", start.elapsed());
                    response.send(fee).unwrap_or_default()
                }
                TickerRequest::GetTokenPrice {
                    token,
                    response,
                    req_type,
                } => {
                    let price = self.get_token_price(token, req_type).await;
                    metrics::histogram!("ticker.get_token_price", start.elapsed());
                    response.send(price).unwrap_or_default();
                }
                TickerRequest::IsTokenAllowed { token, response } => {
                    let allowed = self.validator.token_allowed(token).await;
                    metrics::histogram!("ticker.is_token_allowed", start.elapsed());
                    response.send(allowed).unwrap_or_default();
                }
            }
        }
    }

    async fn get_token_price(
        &self,
        token: TokenLike,
        request_type: TokenPriceRequestType,
    ) -> Result<BigDecimal, anyhow::Error> {
        let factor = match request_type {
            TokenPriceRequestType::USDForOneWei => {
                let token_decimals = self.api.get_token(token.clone()).await?.decimals;
                BigUint::from(10u32).pow(u32::from(token_decimals))
            }
            TokenPriceRequestType::USDForOneToken => BigUint::from(1u32),
        };

        self.api
            .get_last_quote(token)
            .await
            .map(|price| ratio_to_big_decimal(&(price.usd_price / factor), 100))
    }

    /// Returns `true` if the token is subsidized.
    fn is_token_subsidized(&self, token: Token) -> bool {
        // We have disabled the subsidies up until the contract upgrade (when the prices will indeed become that
        // low), but however we want to leave ourselves the possibility to easily enable them if required.
        // Thus:
        // TODO: Remove subsidies completely (ZKS-226)
        let subsidies_enabled = std::env::var("TICKER_SUBSIDIES_ENABLED")
            .map(|val| val == "true")
            .unwrap_or(false);
        if !subsidies_enabled {
            return false;
        }

        !self.config.not_subsidized_tokens.contains(&token.address)
    }

    async fn get_fee_from_ticker_in_wei(
        &mut self,
        tx_type: TxFeeTypes,
        token: TokenLike,
    ) -> Result<Fee, anyhow::Error> {
        let zkp_cost_chunk = self.config.zkp_cost_chunk_usd.clone();
        let token = self.api.get_token(token).await?;
        let token_risk_factor = self
            .config
            .tokens_risk_factors
            .get(&token.id)
            .cloned()
            .unwrap_or_else(|| Ratio::from_integer(1u32.into()));

        let (fee_type, op_chunks) = match tx_type {
            TxFeeTypes::Withdraw => (OutputFeeType::Withdraw, WithdrawOp::CHUNKS),
            TxFeeTypes::FastWithdraw => (OutputFeeType::FastWithdraw, WithdrawOp::CHUNKS),
            TxFeeTypes::Transfer => (OutputFeeType::TransferToNew, TransferToNewOp::CHUNKS),
            TxFeeTypes::ChangePubKey {
                onchain_pubkey_auth,
            } => (
                OutputFeeType::ChangePubKey {
                    onchain_pubkey_auth,
                },
                ChangePubKeyOp::CHUNKS,
            ),
        };
        // Convert chunks amount to `BigUint`.
        let op_chunks = BigUint::from(op_chunks);
        let gas_tx_amount = {
            let is_token_subsidized = self.is_token_subsidized(token.clone());
            if is_token_subsidized {
                self.config
                    .gas_cost_tx
                    .subsidize_cost
                    .get(&fee_type)
                    .cloned()
                    .unwrap()
            } else {
                self.config
                    .gas_cost_tx
                    .standard_cost
                    .get(&fee_type)
                    .cloned()
                    .unwrap()
            }
        };
        let gas_price_wei = self.api.get_gas_price_wei().await?;
        let wei_price_usd = self.api.get_last_quote(TokenLike::Id(0)).await?.usd_price
            / BigUint::from(10u32).pow(18u32);

        let token_price_usd = self
            .api
            .get_last_quote(TokenLike::Id(token.id))
            .await?
            .usd_price
            / BigUint::from(10u32).pow(u32::from(token.decimals));

        let zkp_fee =
            (zkp_cost_chunk * op_chunks) * token_risk_factor.clone() / token_price_usd.clone();
        let gas_fee = (wei_price_usd * gas_tx_amount.clone() * gas_price_wei.clone())
            * token_risk_factor
            / token_price_usd;

        Ok(Fee::new(
            fee_type,
            zkp_fee,
            gas_fee,
            gas_tx_amount,
            gas_price_wei,
        ))
    }
}
