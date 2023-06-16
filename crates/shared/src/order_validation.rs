use {
    crate::{
        account_balances::{BalanceFetching, TransferSimulationError},
        bad_token::{BadTokenDetecting, TokenQuality},
        code_fetching::CodeFetching,
        order_quoting::{
            CalculateQuoteError,
            FindQuoteError,
            OrderQuoting,
            Quote,
            QuoteParameters,
            QuoteSearchParameters,
        },
        price_estimation::{PriceEstimationError, Verification},
        signature_validator::{SignatureCheck, SignatureValidating, SignatureValidationError},
    },
    anyhow::{anyhow, Result},
    async_trait::async_trait,
    contracts::WETH9,
    database::{onchain_broadcasted_orders::OnchainOrderPlacementError, quotes::QuoteKind},
    ethcontract::{H160, H256, U256},
    model::{
        app_id::AppDataHash,
        order::{
            BuyTokenDestination,
            Order,
            OrderClass,
            OrderCreation,
            OrderCreationAppData,
            OrderData,
            OrderKind,
            OrderMetadata,
            SellTokenSource,
            VerificationError,
            BUY_ETH_ADDRESS,
        },
        quote::{OrderQuoteSide, QuoteSigningScheme, SellAmount},
        signature::{self, hashed_eip712_message, Signature, SigningScheme},
        time,
        DomainSeparator,
    },
    std::{collections::HashSet, sync::Arc, time::Duration},
};

#[mockall::automock]
#[async_trait::async_trait]
pub trait OrderValidating: Send + Sync {
    /// Partial (aka Pre-) Validation is aimed at catching malformed order data
    /// during the fee & quote phase (i.e. before the order is signed).
    /// Thus, partial validation *doesn't* verify:
    ///     - signatures
    ///     - user sell balances or fee sufficiency.
    ///
    /// Specifically, but *does* verify:
    ///     - if buy token is native asset, receiver is not a smart contract,
    ///     - the sell token is not the native asset,
    ///     - the sender is not a banned user,
    ///     - the order validity is appropriate,
    ///     - buy_token is not the same as sell_token,
    ///     - buy and sell token destination and source are supported.
    ///     - buy & sell tokens passed "bad token" detection,
    async fn partial_validate(&self, order: PreOrderData) -> Result<(), PartialValidationError>;

    /// This is the full order validation performed at the time of order
    /// placement (i.e. once all the required fields on an Order are
    /// provided). Specifically, verifying that
    ///     - buy & sell amounts are non-zero,
    ///     - order's signature recovers correctly
    ///     - fee is sufficient,
    ///     - user has sufficient (transferable) funds to execute the order.
    ///
    /// Furthermore, full order validation also calls partial_validate to ensure
    /// that other aspects of the order are not malformed.
    ///
    /// `full_app_data_override` can be used when the order specifies only a
    /// contract app data hash without the full app data. In this case
    /// `full_app_data_override` is used as the full app data and the contract
    /// app data hash is not validated against it (the hash doesn't have to
    /// match). The full app data is still otherwise validated.
    async fn validate_and_construct_order(
        &self,
        order: OrderCreation,
        domain_separator: &DomainSeparator,
        settlement_contract: H160,
        full_app_data_override: Option<String>,
    ) -> Result<(Order, Option<Quote>), ValidationError>;
}

#[derive(Debug)]
pub enum PartialValidationError {
    Forbidden,
    ValidTo(OrderValidToError),
    TransferEthToContract,
    InvalidNativeSellToken,
    SameBuyAndSellToken,
    UnsupportedBuyTokenDestination(BuyTokenDestination),
    UnsupportedSellTokenSource(SellTokenSource),
    UnsupportedOrderType,
    UnsupportedSignature,
    UnsupportedToken { token: H160, reason: String },
    Other(anyhow::Error),
}

impl From<OrderValidToError> for PartialValidationError {
    fn from(err: OrderValidToError) -> Self {
        Self::ValidTo(err)
    }
}

#[derive(Debug)]
pub enum ValidationError {
    Partial(PartialValidationError),
    /// The quote ID specified with the order could not be found.
    QuoteNotFound,
    /// The quote specified by ID is invalid. Either it doesn't match the order
    /// or it has already expired.
    InvalidQuote,
    /// Unable to compute quote because of a price estimation error.
    PriceForQuote(PriceEstimationError),
    InsufficientFee,
    InsufficientBalance,
    InsufficientAllowance,
    InvalidSignature,
    /// If fee and sell amount overflow u256
    SellAmountOverflow,
    TransferSimulationFailed,
    /// The specified on-chain signature requires the from address of the
    /// order signer.
    MissingFrom,
    WrongOwner(signature::Recovered),
    /// An invalid EIP-1271 signature, where the on-chain validation check
    /// reverted or did not return the expected value.
    InvalidEip1271Signature(H256),
    ZeroAmount,
    IncompatibleSigningScheme,
    TooManyLimitOrders,
    InvalidAppData(anyhow::Error),
    AppDataHashMismatch {
        provided: AppDataHash,
        actual: AppDataHash,
    },
    Other(anyhow::Error),
}

pub fn onchain_order_placement_error_from(error: ValidationError) -> OnchainOrderPlacementError {
    match error {
        ValidationError::QuoteNotFound => OnchainOrderPlacementError::QuoteNotFound,
        ValidationError::Partial(_) => OnchainOrderPlacementError::PreValidationError,
        ValidationError::InvalidQuote => OnchainOrderPlacementError::InvalidQuote,
        ValidationError::InsufficientFee => OnchainOrderPlacementError::InsufficientFee,
        _ => OnchainOrderPlacementError::Other,
    }
}

impl From<VerificationError> for ValidationError {
    fn from(err: VerificationError) -> Self {
        match err {
            VerificationError::UnableToRecoverSigner(_) => Self::InvalidSignature,
            VerificationError::UnexpectedSigner(recovered) => Self::WrongOwner(recovered),
            VerificationError::MissingFrom => Self::MissingFrom,
        }
    }
}

impl From<FindQuoteError> for ValidationError {
    fn from(err: FindQuoteError) -> Self {
        match err {
            FindQuoteError::NotFound(_) => Self::QuoteNotFound,
            FindQuoteError::ParameterMismatch(_) | FindQuoteError::Expired(_) => Self::InvalidQuote,
            FindQuoteError::Other(err) => Self::Other(err),
        }
    }
}

impl From<CalculateQuoteError> for ValidationError {
    fn from(err: CalculateQuoteError) -> Self {
        match err {
            CalculateQuoteError::Price(PriceEstimationError::UnsupportedToken {
                token,
                reason,
            }) => {
                ValidationError::Partial(PartialValidationError::UnsupportedToken { token, reason })
            }
            CalculateQuoteError::Price(PriceEstimationError::ZeroAmount) => {
                ValidationError::ZeroAmount
            }
            CalculateQuoteError::Other(err)
            | CalculateQuoteError::Price(PriceEstimationError::Other(err)) => {
                ValidationError::Other(err)
            }
            CalculateQuoteError::Price(err) => ValidationError::PriceForQuote(err),
            // This should never happen because we only calculate quotes with
            // `SellAmount::AfterFee`, meaning that the sell amount does not
            // need to be higher than the computed fee amount. Don't bubble up
            // and handle these errors in a general way.
            err @ CalculateQuoteError::SellAmountDoesNotCoverFee { .. } => {
                ValidationError::Other(anyhow!(err).context("unexpected quote calculation error"))
            }
        }
    }
}

#[mockall::automock]
#[async_trait]
pub trait LimitOrderCounting: Send + Sync {
    async fn count(&self, owner: H160) -> Result<u64>;
}

#[derive(Clone)]
pub struct OrderValidator {
    /// For Pre/Partial-Validation: performed during fee & quote phase
    /// when only part of the order data is available
    native_token: WETH9,
    banned_users: HashSet<H160>,
    liquidity_order_owners: HashSet<H160>,
    validity_configuration: OrderValidPeriodConfiguration,
    signature_configuration: SignatureConfiguration,
    bad_token_detector: Arc<dyn BadTokenDetecting>,
    /// For Full-Validation: performed time of order placement
    quoter: Arc<dyn OrderQuoting>,
    balance_fetcher: Arc<dyn BalanceFetching>,
    signature_validator: Arc<dyn SignatureValidating>,
    enable_fill_or_kill_limit_orders: bool,
    enable_partially_fillable_limit_orders: bool,
    limit_order_counter: Arc<dyn LimitOrderCounting>,
    max_limit_orders_per_user: u64,
    pub code_fetcher: Arc<dyn CodeFetching>,
    pub enable_eth_smart_contract_payments: bool,
    enable_custom_interactions: bool,
    app_data_validator: crate::app_data::Validator,
    request_verified_quotes: bool,
}

#[derive(Debug, Eq, PartialEq, Default)]
pub struct PreOrderData {
    pub owner: H160,
    pub sell_token: H160,
    pub buy_token: H160,
    pub receiver: H160,
    pub valid_to: u32,
    pub partially_fillable: bool,
    pub buy_token_balance: BuyTokenDestination,
    pub sell_token_balance: SellTokenSource,
    pub signing_scheme: SigningScheme,
    pub class: OrderClass,
}

fn actual_receiver(owner: H160, order: &OrderData) -> H160 {
    let receiver = order.receiver.unwrap_or_default();
    if receiver == H160::zero() {
        owner
    } else {
        receiver
    }
}

impl PreOrderData {
    pub fn from_order_creation(
        owner: H160,
        order: &OrderData,
        signing_scheme: SigningScheme,
        liquidity_owner: bool,
    ) -> Self {
        Self {
            owner,
            sell_token: order.sell_token,
            buy_token: order.buy_token,
            receiver: actual_receiver(owner, order),
            valid_to: order.valid_to,
            partially_fillable: order.partially_fillable,
            buy_token_balance: order.buy_token_balance,
            sell_token_balance: order.sell_token_balance,
            signing_scheme,
            class: match (liquidity_owner, order.fee_amount.is_zero()) {
                (false, false) => OrderClass::Market,
                (false, true) => OrderClass::Limit(Default::default()),
                (true, _) => OrderClass::Liquidity,
            },
        }
    }
}

impl OrderValidator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        native_token: WETH9,
        banned_users: HashSet<H160>,
        liquidity_order_owners: HashSet<H160>,
        validity_configuration: OrderValidPeriodConfiguration,
        signature_configuration: SignatureConfiguration,
        bad_token_detector: Arc<dyn BadTokenDetecting>,
        quoter: Arc<dyn OrderQuoting>,
        balance_fetcher: Arc<dyn BalanceFetching>,
        signature_validator: Arc<dyn SignatureValidating>,
        limit_order_counter: Arc<dyn LimitOrderCounting>,
        max_limit_orders_per_user: u64,
        code_fetcher: Arc<dyn CodeFetching>,
        app_data_validator: crate::app_data::Validator,
    ) -> Self {
        Self {
            native_token,
            banned_users,
            liquidity_order_owners,
            validity_configuration,
            signature_configuration,
            bad_token_detector,
            quoter,
            balance_fetcher,
            signature_validator,
            enable_fill_or_kill_limit_orders: false,
            enable_partially_fillable_limit_orders: false,
            limit_order_counter,
            max_limit_orders_per_user,
            code_fetcher,
            enable_eth_smart_contract_payments: false,
            enable_custom_interactions: false,
            app_data_validator,
            request_verified_quotes: false,
        }
    }

    pub fn with_fill_or_kill_limit_orders(mut self, enable: bool) -> Self {
        self.enable_fill_or_kill_limit_orders = enable;
        self
    }

    pub fn with_partially_fillable_limit_orders(mut self, enable: bool) -> Self {
        self.enable_partially_fillable_limit_orders = enable;
        self
    }

    pub fn with_eth_smart_contract_payments(mut self, enable: bool) -> Self {
        self.enable_eth_smart_contract_payments = enable;
        self
    }

    pub fn with_custom_interactions(mut self, enable: bool) -> Self {
        self.enable_custom_interactions = enable;
        self
    }

    pub fn with_verified_quotes(mut self, enable: bool) -> Self {
        self.request_verified_quotes = enable;
        self
    }

    async fn check_max_limit_orders(
        &self,
        owner: H160,
        class: &OrderClass,
    ) -> Result<(), ValidationError> {
        if class.is_limit() {
            let num_limit_orders = self
                .limit_order_counter
                .count(owner)
                .await
                .map_err(ValidationError::Other)?;
            if num_limit_orders >= self.max_limit_orders_per_user {
                return Err(ValidationError::TooManyLimitOrders);
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl OrderValidating for OrderValidator {
    async fn partial_validate(&self, order: PreOrderData) -> Result<(), PartialValidationError> {
        if self.banned_users.contains(&order.owner) || self.banned_users.contains(&order.receiver) {
            return Err(PartialValidationError::Forbidden);
        }

        match order.class {
            OrderClass::Market => {
                if order.partially_fillable {
                    return Err(PartialValidationError::UnsupportedOrderType);
                }
            }
            OrderClass::Limit(_) => {
                if order.partially_fillable && !self.enable_partially_fillable_limit_orders {
                    return Err(PartialValidationError::UnsupportedOrderType);
                }
                if !order.partially_fillable && !self.enable_fill_or_kill_limit_orders {
                    return Err(PartialValidationError::UnsupportedOrderType);
                }
            }
            OrderClass::Liquidity => (),
        }

        if order.buy_token_balance != BuyTokenDestination::Erc20 {
            return Err(PartialValidationError::UnsupportedBuyTokenDestination(
                order.buy_token_balance,
            ));
        }
        if !matches!(
            order.sell_token_balance,
            SellTokenSource::Erc20 | SellTokenSource::External
        ) {
            return Err(PartialValidationError::UnsupportedSellTokenSource(
                order.sell_token_balance,
            ));
        }

        self.validity_configuration.validate_period(&order)?;

        // Eventually we will support all Signature types and can remove this.
        if !self
            .signature_configuration
            .is_signing_scheme_supported(order.signing_scheme)
        {
            return Err(PartialValidationError::UnsupportedSignature);
        }

        if has_same_buy_and_sell_token(&order, &self.native_token) {
            return Err(PartialValidationError::SameBuyAndSellToken);
        }
        if order.sell_token == BUY_ETH_ADDRESS {
            return Err(PartialValidationError::InvalidNativeSellToken);
        }
        if !self.enable_eth_smart_contract_payments && order.buy_token == BUY_ETH_ADDRESS {
            let code_size = self
                .code_fetcher
                .code_size(order.receiver)
                .await
                .map_err(PartialValidationError::Other)?;
            if code_size != 0 {
                return Err(PartialValidationError::TransferEthToContract);
            }
        }

        for &token in &[order.sell_token, order.buy_token] {
            if let TokenQuality::Bad { reason } = self
                .bad_token_detector
                .detect(token)
                .await
                .map_err(PartialValidationError::Other)?
            {
                return Err(PartialValidationError::UnsupportedToken { token, reason });
            }
        }

        Ok(())
    }

    async fn validate_and_construct_order(
        &self,
        order: OrderCreation,
        domain_separator: &DomainSeparator,
        settlement_contract: H160,
        full_app_data_override: Option<String>,
    ) -> Result<(Order, Option<Quote>), ValidationError> {
        // Happens before signature verification because a miscalculated app data hash
        // by the API user would lead to being unable to validate the signature below.
        let validate = |app_data: &String| {
            self.app_data_validator
                .validate(app_data.as_bytes())
                .map_err(ValidationError::InvalidAppData)
        };
        let app_data_hash = match &order.app_data {
            OrderCreationAppData::Both { full, expected } => {
                let validated = validate(full)?;
                if validated.hash != *expected {
                    return Err(ValidationError::AppDataHashMismatch {
                        provided: *expected,
                        actual: validated.hash,
                    });
                }
                validated.hash
            }
            OrderCreationAppData::Hash { hash } => {
                // Eventually we're not going to accept orders that set only a hash and where we
                // can't find full app data elsewhere.
                if let Some(full) = &full_app_data_override {
                    validate(full)?;
                }
                *hash
            }
            OrderCreationAppData::Full { full } => validate(full)?.hash,
        };

        let owner = order.verify_owner(domain_separator)?;
        let signing_scheme = order.signature.scheme();
        let data = OrderData {
            app_data: app_data_hash,
            ..order.data()
        };
        let uid = data.uid(domain_separator, &owner);

        let additional_gas = if let Signature::Eip1271(signature) = &order.signature {
            if self
                .signature_configuration
                .eip1271_skip_creation_validation
            {
                tracing::debug!(?signature, "skipping EIP-1271 signature validation");
                // We don't care! Because we are skipping validation anyway
                0u64
            } else {
                let hash = hashed_eip712_message(domain_separator, &data.hash_struct());
                self.signature_validator
                    .validate_signature_and_get_additional_gas(SignatureCheck {
                        signer: owner,
                        hash,
                        signature: signature.to_owned(),
                    })
                    .await
                    .map_err(|err| match err {
                        SignatureValidationError::Invalid => {
                            ValidationError::InvalidEip1271Signature(H256(hash))
                        }
                        SignatureValidationError::Other(err) => ValidationError::Other(err),
                    })?
            }
        } else {
            // in any other case, just apply 0
            0u64
        };

        if data.buy_amount.is_zero() || data.sell_amount.is_zero() {
            return Err(ValidationError::ZeroAmount);
        }

        let pre_order = PreOrderData::from_order_creation(
            owner,
            &data,
            signing_scheme,
            self.liquidity_order_owners.contains(&owner),
        );
        let class = pre_order.class;
        self.partial_validate(pre_order)
            .await
            .map_err(ValidationError::Partial)?;

        let verification = self.request_verified_quotes.then_some(Verification {
            from: owner,
            receiver: order.receiver.unwrap_or(owner),
            sell_token_source: order.sell_token_balance,
            buy_token_destination: order.buy_token_balance,
            // TODO get these from the request
            pre_interactions: vec![],
            post_interactions: vec![],
        });

        let quote_parameters = QuoteSearchParameters {
            sell_token: data.sell_token,
            buy_token: data.buy_token,
            sell_amount: data.sell_amount,
            buy_amount: data.buy_amount,
            fee_amount: data.fee_amount,
            kind: data.kind,
            verification,
        };
        let quote = if class == OrderClass::Market {
            let quote = get_quote_and_check_fee(
                &*self.quoter,
                &quote_parameters,
                order.quote_id,
                data.fee_amount,
                convert_signing_scheme_into_quote_signing_scheme(
                    order.signature.scheme(),
                    true,
                    additional_gas,
                )?,
            )
            .await?;
            Some(quote)
        } else {
            // We don't try to get quotes for liquidity and limit orders
            // for two reasons:
            // 1. They don't pay fees, meaning we don't need to know what the
            //    min fee amount is.
            // 2. We don't really care about the equivalent quote since they
            //    aren't expected to follow regular order creation flow.
            None
        };

        let full_fee_amount = quote
            .as_ref()
            .map(|quote| quote.full_fee_amount)
            // The `full_fee_amount` should never be lower than the `fee_amount` (which may include
            // subsidies). This only makes a difference for liquidity orders.
            .unwrap_or(data.fee_amount);

        let min_balance = minimum_balance(&data).ok_or(ValidationError::SellAmountOverflow)?;

        // Fast path to check if transfer is possible with a single node query.
        // If not, run extra queries for additional information.
        match self
            .balance_fetcher
            .can_transfer(data.sell_token, owner, min_balance, data.sell_token_balance)
            .await
        {
            Ok(_) => (),
            Err(
                TransferSimulationError::InsufficientAllowance
                | TransferSimulationError::InsufficientBalance
                | TransferSimulationError::TransferFailed,
            ) if signing_scheme == SigningScheme::PreSign => {
                // We have an exception for pre-sign orders where they do not
                // require sufficient balance or allowance. The idea, is that
                // this allows smart contracts to place orders bundled with
                // other transactions that either produce the required balance
                // or set the allowance. This would, for example, allow a Gnosis
                // Safe to bundle the pre-signature transaction with a WETH wrap
                // and WETH approval to the vault relayer contract.
            }
            Err(err) => match err {
                TransferSimulationError::InsufficientAllowance => {
                    return Err(ValidationError::InsufficientAllowance);
                }
                TransferSimulationError::InsufficientBalance => {
                    return Err(ValidationError::InsufficientBalance);
                }
                TransferSimulationError::TransferFailed => {
                    return Err(ValidationError::TransferSimulationFailed);
                }
                TransferSimulationError::Other(err) => {
                    tracing::warn!("TransferSimulation failed: {:?}", err);
                    return Err(ValidationError::TransferSimulationFailed);
                }
            },
        }

        // Check if we need to re-classify the order if it is outside the market
        // price. We consider out-of-price orders as liquidity orders. See
        // <https://github.com/cowprotocol/services/pull/301>.
        let class = match &quote {
            Some(quote)
                if is_order_outside_market_price(
                    &quote_parameters.sell_amount,
                    &quote_parameters.buy_amount,
                    quote,
                ) =>
            {
                tracing::debug!(%uid, ?owner, ?class, "order being flagged as outside market price");
                OrderClass::Liquidity
            }
            _ => class,
        };

        self.check_max_limit_orders(owner, &class).await?;

        let order = Order {
            metadata: OrderMetadata {
                owner,
                creation_date: chrono::offset::Utc::now(),
                uid,
                settlement_contract,
                full_fee_amount,
                class,
                full_app_data: match order.app_data {
                    OrderCreationAppData::Both { full, .. }
                    | OrderCreationAppData::Full { full } => Some(full),
                    OrderCreationAppData::Hash { .. } => full_app_data_override,
                },
                ..Default::default()
            },
            signature: order.signature.clone(),
            data,
            interactions: Default::default(),
        };

        Ok((order, quote))
    }
}

/// Order validity period configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrderValidPeriodConfiguration {
    pub min: Duration,
    pub max_market: Duration,
    pub max_limit: Duration,
}

impl OrderValidPeriodConfiguration {
    /// Creates an configuration where any `validTo` is accepted.
    pub fn any() -> Self {
        Self {
            min: Duration::ZERO,
            max_market: Duration::MAX,
            max_limit: Duration::MAX,
        }
    }

    /// Validates an order's timestamp based on additional data.
    fn validate_period(&self, order: &PreOrderData) -> Result<(), OrderValidToError> {
        let now = time::now_in_epoch_seconds();
        if order.valid_to < time::timestamp_after_duration(now, self.min) {
            return Err(OrderValidToError::Insufficient);
        }
        if order.valid_to > time::timestamp_after_duration(now, self.max(order)) {
            return Err(OrderValidToError::Excessive);
        }

        Ok(())
    }

    /// Returns the maximum valid timestamp for the specified order.
    fn max(&self, order: &PreOrderData) -> Duration {
        // For now, there is no maximum `validTo` for pre-sign orders as a hack
        // for dealing with signature collection times. We should probably
        // revisit this.
        if order.signing_scheme == SigningScheme::PreSign {
            return Duration::MAX;
        }

        match order.class {
            OrderClass::Market => self.max_market,
            OrderClass::Limit(_) => self.max_limit,
            OrderClass::Liquidity => Duration::MAX,
        }
    }
}

#[derive(Debug)]
pub enum OrderValidToError {
    Insufficient,
    Excessive,
}

/// Signature configuration that is accepted by the orderbook.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignatureConfiguration {
    pub eip1271: bool,
    pub eip1271_skip_creation_validation: bool,
    pub presign: bool,
}

impl SignatureConfiguration {
    /// Returns a configuration where only off-chain signing schemes are
    /// supported.
    pub fn off_chain() -> Self {
        Self {
            eip1271: false,
            eip1271_skip_creation_validation: false,
            presign: false,
        }
    }

    /// Returns a configuration where all signing schemes are enabled.
    pub fn all() -> Self {
        Self {
            eip1271: true,
            eip1271_skip_creation_validation: false,
            presign: true,
        }
    }

    /// returns whether the supplied signature scheme is supported.
    pub fn is_signing_scheme_supported(&self, signing_scheme: SigningScheme) -> bool {
        match signing_scheme {
            SigningScheme::Eip712 | SigningScheme::EthSign => true,
            SigningScheme::Eip1271 => self.eip1271,
            SigningScheme::PreSign => self.presign,
        }
    }
}

/// Returns true if the orders have same buy and sell tokens.
///
/// This also checks for orders selling wrapped native token for native token.
fn has_same_buy_and_sell_token(order: &PreOrderData, native_token: &WETH9) -> bool {
    order.sell_token == order.buy_token
        || (order.sell_token == native_token.address() && order.buy_token == BUY_ETH_ADDRESS)
}

/// Min balance user must have in sell token for order to be accepted.
///
/// None when addition overflows.
fn minimum_balance(order: &OrderData) -> Option<U256> {
    // TODO: We might even want to allow 0 balance for partially fillable but we
    // require balance for fok limit orders too so this make some sense and protects
    // against accidentally creating order for token without balance.
    if order.partially_fillable {
        return Some(1.into());
    }
    order.sell_amount.checked_add(order.fee_amount)
}

/// Retrieves the quote for an order that is being created and verify that its
/// fee is sufficient.
///
/// This works by first trying to find an existing quote, and then falling back
/// to calculating a brand new one if none can be found and a quote ID was not
/// specified.
pub async fn get_quote_and_check_fee(
    quoter: &dyn OrderQuoting,
    quote_search_parameters: &QuoteSearchParameters,
    quote_id: Option<i64>,
    fee_amount: U256,
    signing_scheme: QuoteSigningScheme,
) -> Result<Quote, ValidationError> {
    let quote = match quoter
        .find_quote(quote_id, quote_search_parameters.clone(), &signing_scheme)
        .await
    {
        Ok(quote) => {
            tracing::debug!(quote_id =? quote.id, "found quote for order creation");
            quote
        }
        // We couldn't find a quote, and no ID was specified. Try computing a
        // fresh quote to use instead.
        Err(FindQuoteError::NotFound(_)) if quote_id.is_none() => {
            let parameters = QuoteParameters {
                sell_token: quote_search_parameters.sell_token,
                buy_token: quote_search_parameters.buy_token,
                side: match quote_search_parameters.kind {
                    OrderKind::Buy => OrderQuoteSide::Buy {
                        buy_amount_after_fee: quote_search_parameters.buy_amount,
                    },
                    OrderKind::Sell => OrderQuoteSide::Sell {
                        sell_amount: SellAmount::AfterFee {
                            value: quote_search_parameters.sell_amount,
                        },
                    },
                },
                verification: quote_search_parameters.verification.clone(),
                signing_scheme,
            };

            let quote = quoter.calculate_quote(parameters).await?;
            let quote = quoter
                .store_quote(quote)
                .await
                .map_err(ValidationError::Other)?;

            tracing::debug!(quote_id =? quote.id, "computed fresh quote for order creation");
            quote
        }
        Err(err) => return Err(err.into()),
    };

    if fee_amount < quote.fee_amount {
        return Err(ValidationError::InsufficientFee);
    }

    Ok(quote)
}

/// Checks whether or not an order's limit price is outside the market price
/// specified by the quote.
///
/// Note that this check only looks at the order's limit price and the market
/// price and is independent of amounts or trade direction.
pub fn is_order_outside_market_price(sell_amount: &U256, buy_amount: &U256, quote: &Quote) -> bool {
    sell_amount.full_mul(quote.buy_amount) < quote.sell_amount.full_mul(*buy_amount)
}

pub fn convert_signing_scheme_into_quote_signing_scheme(
    scheme: SigningScheme,
    order_placement_via_api: bool,
    verification_gas_limit: u64,
) -> Result<QuoteSigningScheme, ValidationError> {
    match (order_placement_via_api, scheme) {
        (true, SigningScheme::Eip712) => Ok(QuoteSigningScheme::Eip712),
        (true, SigningScheme::EthSign) => Ok(QuoteSigningScheme::EthSign),
        (false, SigningScheme::Eip712) => Err(ValidationError::IncompatibleSigningScheme),
        (false, SigningScheme::EthSign) => Err(ValidationError::IncompatibleSigningScheme),
        (order_placement_via_api, SigningScheme::PreSign) => Ok(QuoteSigningScheme::PreSign {
            onchain_order: !order_placement_via_api,
        }),
        (order_placement_via_api, SigningScheme::Eip1271) => Ok(QuoteSigningScheme::Eip1271 {
            onchain_order: !order_placement_via_api,
            verification_gas_limit,
        }),
    }
}

pub fn convert_signing_scheme_into_quote_kind(
    scheme: SigningScheme,
    order_placement_via_api: bool,
) -> Result<QuoteKind, ValidationError> {
    match order_placement_via_api {
        true => Ok(QuoteKind::Standard),
        false => match scheme {
            SigningScheme::Eip1271 => Ok(QuoteKind::Eip1271OnchainOrder),
            SigningScheme::PreSign => Ok(QuoteKind::PreSignOnchainOrder),
            _ => Err(ValidationError::IncompatibleSigningScheme),
        },
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            account_balances::MockBalanceFetching,
            bad_token::{MockBadTokenDetecting, TokenQuality},
            code_fetching::MockCodeFetching,
            dummy_contract,
            order_quoting::MockOrderQuoting,
            signature_validator::MockSignatureValidating,
        },
        anyhow::anyhow,
        chrono::Utc,
        ethcontract::web3::signing::SecretKeyRef,
        futures::FutureExt,
        maplit::hashset,
        mockall::predicate::{always, eq},
        model::{
            quote::default_verification_gas_limit,
            signature::{EcdsaSignature, EcdsaSigningScheme},
        },
        secp256k1::ONE_KEY,
    };

    #[test]
    fn minimum_balance_() {
        let order = OrderData {
            sell_amount: U256::MAX,
            fee_amount: U256::from(1),
            ..Default::default()
        };
        assert_eq!(minimum_balance(&order), None);
        let order = OrderData {
            sell_amount: U256::from(1),
            fee_amount: U256::from(1),
            ..Default::default()
        };
        assert_eq!(minimum_balance(&order), Some(U256::from(2)));
    }

    #[test]
    fn detects_orders_with_same_buy_and_sell_token() {
        let native_token = dummy_contract!(WETH9, [0xef; 20]);
        assert!(has_same_buy_and_sell_token(
            &PreOrderData {
                sell_token: H160([0x01; 20]),
                buy_token: H160([0x01; 20]),
                ..Default::default()
            },
            &native_token,
        ));
        assert!(has_same_buy_and_sell_token(
            &PreOrderData {
                sell_token: native_token.address(),
                buy_token: BUY_ETH_ADDRESS,
                ..Default::default()
            },
            &native_token,
        ));

        assert!(!has_same_buy_and_sell_token(
            &PreOrderData {
                sell_token: H160([0x01; 20]),
                buy_token: H160([0x02; 20]),
                ..Default::default()
            },
            &native_token,
        ));
        // Sell token set to 0xeee...eee has no special meaning, so it isn't
        // considered buying and selling the same token.
        assert!(!has_same_buy_and_sell_token(
            &PreOrderData {
                sell_token: BUY_ETH_ADDRESS,
                buy_token: native_token.address(),
                ..Default::default()
            },
            &native_token,
        ));
    }

    #[tokio::test]
    async fn pre_validate_err() {
        let native_token = dummy_contract!(WETH9, [0xef; 20]);
        let validity_configuration = OrderValidPeriodConfiguration {
            min: Duration::from_secs(1),
            max_market: Duration::from_secs(100),
            max_limit: Duration::from_secs(200),
        };
        let banned_users = hashset![H160::from_low_u64_be(1)];
        let legit_valid_to =
            time::now_in_epoch_seconds() + validity_configuration.min.as_secs() as u32 + 2;
        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter.expect_count().returning(|_| Ok(0u64));
        let validator = OrderValidator::new(
            native_token,
            banned_users,
            hashset!(),
            validity_configuration,
            SignatureConfiguration::off_chain(),
            Arc::new(MockBadTokenDetecting::new()),
            Arc::new(MockOrderQuoting::new()),
            Arc::new(MockBalanceFetching::new()),
            Arc::new(MockSignatureValidating::new()),
            Arc::new(limit_order_counter),
            0,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        );
        let result = validator
            .partial_validate(PreOrderData {
                partially_fillable: true,
                ..Default::default()
            })
            .await;
        assert!(
            matches!(result, Err(PartialValidationError::UnsupportedOrderType)),
            "{result:?}"
        );
        assert!(matches!(
            validator
                .partial_validate(PreOrderData {
                    owner: H160::from_low_u64_be(1),
                    ..Default::default()
                })
                .await,
            Err(PartialValidationError::Forbidden)
        ));
        assert!(matches!(
            validator
                .partial_validate(PreOrderData {
                    receiver: H160::from_low_u64_be(1),
                    ..Default::default()
                })
                .await,
            Err(PartialValidationError::Forbidden)
        ));
        assert!(matches!(
            validator
                .partial_validate(PreOrderData {
                    buy_token_balance: BuyTokenDestination::Internal,
                    ..Default::default()
                })
                .await,
            Err(PartialValidationError::UnsupportedBuyTokenDestination(
                BuyTokenDestination::Internal
            ))
        ));
        assert!(matches!(
            validator
                .partial_validate(PreOrderData {
                    sell_token_balance: SellTokenSource::Internal,
                    ..Default::default()
                })
                .await,
            Err(PartialValidationError::UnsupportedSellTokenSource(
                SellTokenSource::Internal
            ))
        ));
        assert!(matches!(
            validator
                .partial_validate(PreOrderData {
                    valid_to: 0,
                    ..Default::default()
                })
                .await,
            Err(PartialValidationError::ValidTo(
                OrderValidToError::Insufficient,
            ))
        ));
        assert!(matches!(
            validator
                .partial_validate(PreOrderData {
                    valid_to: legit_valid_to
                        + validity_configuration.max_market.as_secs() as u32
                        + 1,
                    ..Default::default()
                })
                .await,
            Err(PartialValidationError::ValidTo(
                OrderValidToError::Excessive,
            ))
        ));
        {
            let validator = validator.clone().with_fill_or_kill_limit_orders(true);
            assert!(matches!(
                validator
                    .partial_validate(PreOrderData {
                        valid_to: legit_valid_to
                            + validity_configuration.max_limit.as_secs() as u32
                            + 1,
                        class: OrderClass::Limit(Default::default()),
                        ..Default::default()
                    })
                    .await,
                Err(PartialValidationError::ValidTo(
                    OrderValidToError::Excessive,
                ))
            ));
        }
        assert!(matches!(
            validator
                .partial_validate(PreOrderData {
                    valid_to: legit_valid_to,
                    buy_token: H160::from_low_u64_be(2),
                    sell_token: H160::from_low_u64_be(2),
                    ..Default::default()
                })
                .await,
            Err(PartialValidationError::SameBuyAndSellToken)
        ));
        assert!(matches!(
            validator
                .partial_validate(PreOrderData {
                    valid_to: legit_valid_to,
                    sell_token: BUY_ETH_ADDRESS,
                    ..Default::default()
                })
                .await,
            Err(PartialValidationError::InvalidNativeSellToken)
        ));
        assert!(matches!(
            validator
                .partial_validate(PreOrderData {
                    valid_to: legit_valid_to,
                    signing_scheme: SigningScheme::PreSign,
                    ..Default::default()
                })
                .await,
            Err(PartialValidationError::UnsupportedSignature)
        ));
    }

    #[tokio::test]
    async fn pre_validate_ok() {
        let liquidity_order_owner = H160::from_low_u64_be(0x42);
        let validity_configuration = OrderValidPeriodConfiguration {
            min: Duration::from_secs(1),
            max_market: Duration::from_secs(100),
            max_limit: Duration::from_secs(200),
        };

        let mut bad_token_detector = MockBadTokenDetecting::new();
        bad_token_detector
            .expect_detect()
            .with(eq(H160::from_low_u64_be(1)))
            .returning(|_| Ok(TokenQuality::Good));
        bad_token_detector
            .expect_detect()
            .with(eq(H160::from_low_u64_be(2)))
            .returning(|_| Ok(TokenQuality::Good));

        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter.expect_count().returning(|_| Ok(0u64));
        let validator = OrderValidator::new(
            dummy_contract!(WETH9, [0xef; 20]),
            hashset!(),
            hashset!(liquidity_order_owner),
            validity_configuration,
            SignatureConfiguration::all(),
            Arc::new(bad_token_detector),
            Arc::new(MockOrderQuoting::new()),
            Arc::new(MockBalanceFetching::new()),
            Arc::new(MockSignatureValidating::new()),
            Arc::new(limit_order_counter),
            0,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        )
        .with_fill_or_kill_limit_orders(true)
        .with_partially_fillable_limit_orders(true);
        let order = || PreOrderData {
            valid_to: time::now_in_epoch_seconds()
                + validity_configuration.min.as_secs() as u32
                + 2,
            sell_token: H160::from_low_u64_be(1),
            buy_token: H160::from_low_u64_be(2),
            ..Default::default()
        };

        assert!(validator.partial_validate(order()).await.is_ok());
        assert!(validator
            .partial_validate(PreOrderData {
                valid_to: u32::MAX,
                signing_scheme: SigningScheme::PreSign,
                ..order()
            })
            .await
            .is_ok());
        assert!(validator
            .partial_validate(PreOrderData {
                class: OrderClass::Limit(Default::default()),
                owner: liquidity_order_owner,
                valid_to: time::now_in_epoch_seconds()
                    + validity_configuration.max_market.as_secs() as u32
                    + 2,
                ..order()
            })
            .await
            .is_ok());
        assert!(validator
            .partial_validate(PreOrderData {
                partially_fillable: true,
                class: OrderClass::Liquidity,
                owner: liquidity_order_owner,
                valid_to: u32::MAX,
                ..order()
            })
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn post_validate_ok() {
        let mut order_quoter = MockOrderQuoting::new();
        let mut bad_token_detector = MockBadTokenDetecting::new();
        let mut balance_fetcher = MockBalanceFetching::new();
        order_quoter
            .expect_find_quote()
            .returning(|_, _, _| Ok(Default::default()));
        bad_token_detector
            .expect_detect()
            .returning(|_| Ok(TokenQuality::Good));
        balance_fetcher
            .expect_can_transfer()
            .returning(|_, _, _, _| Ok(()));

        let mut signature_validating = MockSignatureValidating::new();
        signature_validating
            .expect_validate_signature_and_get_additional_gas()
            .never();
        let signature_validating = Arc::new(signature_validating);

        let max_limit_orders_per_user = 1;

        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter.expect_count().returning(|_| Ok(0u64));
        let validator = OrderValidator::new(
            dummy_contract!(WETH9, [0xef; 20]),
            hashset!(),
            hashset!(),
            OrderValidPeriodConfiguration {
                min: Duration::from_secs(1),
                max_market: Duration::from_secs(100),
                max_limit: Duration::from_secs(200),
            },
            SignatureConfiguration::all(),
            Arc::new(bad_token_detector),
            Arc::new(order_quoter),
            Arc::new(balance_fetcher),
            signature_validating,
            Arc::new(limit_order_counter),
            max_limit_orders_per_user,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        );

        let creation = OrderCreation {
            valid_to: time::now_in_epoch_seconds() + 2,
            sell_token: H160::from_low_u64_be(1),
            buy_token: H160::from_low_u64_be(2),
            buy_amount: U256::from(1),
            sell_amount: U256::from(1),
            fee_amount: U256::from(1),
            signature: Signature::Eip712(EcdsaSignature::non_zero()),
            ..Default::default()
        };
        validator
            .validate_and_construct_order(
                creation.clone(),
                &Default::default(),
                Default::default(),
                None,
            )
            .await
            .unwrap();

        let domain_separator = DomainSeparator::default();
        let creation = OrderCreation {
            from: Some(H160([1; 20])),
            signature: Signature::Eip1271(vec![1, 2, 3]),
            ..creation
        };
        let order_hash = hashed_eip712_message(&domain_separator, &creation.data().hash_struct());

        let mut signature_validator = MockSignatureValidating::new();
        signature_validator
            .expect_validate_signature_and_get_additional_gas()
            .with(eq(SignatureCheck {
                signer: creation.from.unwrap(),
                hash: order_hash,
                signature: vec![1, 2, 3],
            }))
            .returning(|_| Ok(0u64));

        let validator = OrderValidator {
            signature_validator: Arc::new(signature_validator),
            ..validator
        };

        assert!(validator
            .validate_and_construct_order(
                creation.clone(),
                &domain_separator,
                Default::default(),
                None
            )
            .await
            .is_ok());

        let mut signature_validator = MockSignatureValidating::new();
        signature_validator
            .expect_validate_signature_and_get_additional_gas()
            .with(eq(SignatureCheck {
                signer: creation.from.unwrap(),
                hash: order_hash,
                signature: vec![1, 2, 3],
            }))
            .returning(|_| Err(SignatureValidationError::Invalid));

        let validator = OrderValidator {
            signature_validator: Arc::new(signature_validator),
            signature_configuration: SignatureConfiguration {
                eip1271_skip_creation_validation: true,
                ..SignatureConfiguration::all()
            },
            ..validator
        };

        assert!(validator
            .validate_and_construct_order(
                creation.clone(),
                &domain_separator,
                Default::default(),
                None
            )
            .await
            .is_ok());

        let creation_ = OrderCreation {
            fee_amount: U256::zero(),
            ..creation.clone()
        };
        let validator = validator.with_fill_or_kill_limit_orders(true);
        let (order, quote) = validator
            .validate_and_construct_order(creation_, &domain_separator, Default::default(), None)
            .await
            .unwrap();
        assert_eq!(quote, None);
        assert!(order.metadata.class.is_limit());

        let creation_ = OrderCreation {
            fee_amount: U256::zero(),
            partially_fillable: true,
            ..creation
        };
        let validator = validator.with_partially_fillable_limit_orders(true);
        let (order, quote) = validator
            .validate_and_construct_order(creation_, &domain_separator, Default::default(), None)
            .await
            .unwrap();
        assert_eq!(quote, None);
        assert!(order.metadata.class.is_limit());
    }

    #[tokio::test]
    async fn post_validate_too_many_limit_orders() {
        let mut order_quoter = MockOrderQuoting::new();
        let mut bad_token_detector = MockBadTokenDetecting::new();
        let mut balance_fetcher = MockBalanceFetching::new();
        order_quoter
            .expect_find_quote()
            .returning(|_, _, _| Ok(Default::default()));
        bad_token_detector
            .expect_detect()
            .returning(|_| Ok(TokenQuality::Good));
        balance_fetcher
            .expect_can_transfer()
            .returning(|_, _, _, _| Ok(()));

        let mut signature_validating = MockSignatureValidating::new();
        signature_validating
            .expect_validate_signature_and_get_additional_gas()
            .never();
        let signature_validating = Arc::new(signature_validating);

        const MAX_LIMIT_ORDERS_PER_USER: u64 = 2;

        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter
            .expect_count()
            .returning(|_| Ok(MAX_LIMIT_ORDERS_PER_USER));

        let validator = OrderValidator::new(
            dummy_contract!(WETH9, [0xef; 20]),
            hashset!(),
            hashset!(),
            OrderValidPeriodConfiguration::any(),
            SignatureConfiguration::all(),
            Arc::new(bad_token_detector),
            Arc::new(order_quoter),
            Arc::new(balance_fetcher),
            signature_validating,
            Arc::new(limit_order_counter),
            MAX_LIMIT_ORDERS_PER_USER,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        )
        .with_fill_or_kill_limit_orders(true);

        let creation = OrderCreation {
            valid_to: model::time::now_in_epoch_seconds() + 2,
            sell_token: H160::from_low_u64_be(1),
            buy_token: H160::from_low_u64_be(2),
            buy_amount: U256::from(1),
            sell_amount: U256::from(1),
            signature: Signature::Eip712(EcdsaSignature::non_zero()),
            ..Default::default()
        };
        let res = validator
            .validate_and_construct_order(
                creation.clone(),
                &Default::default(),
                Default::default(),
                None,
            )
            .await;
        assert!(
            matches!(res, Err(ValidationError::TooManyLimitOrders)),
            "{res:?}"
        );
    }

    #[tokio::test]
    async fn post_validate_err_zero_amount() {
        let mut order_quoter = MockOrderQuoting::new();
        let mut bad_token_detector = MockBadTokenDetecting::new();
        let mut balance_fetcher = MockBalanceFetching::new();
        order_quoter
            .expect_find_quote()
            .returning(|_, _, _| Ok(Default::default()));
        bad_token_detector
            .expect_detect()
            .returning(|_| Ok(TokenQuality::Good));
        balance_fetcher
            .expect_can_transfer()
            .returning(|_, _, _, _| Ok(()));
        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter.expect_count().returning(|_| Ok(0u64));
        let validator = OrderValidator::new(
            dummy_contract!(WETH9, [0xef; 20]),
            hashset!(),
            hashset!(),
            OrderValidPeriodConfiguration::any(),
            SignatureConfiguration::all(),
            Arc::new(bad_token_detector),
            Arc::new(order_quoter),
            Arc::new(balance_fetcher),
            Arc::new(MockSignatureValidating::new()),
            Arc::new(limit_order_counter),
            0,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        );
        let order = OrderCreation {
            valid_to: time::now_in_epoch_seconds() + 2,
            sell_token: H160::from_low_u64_be(1),
            buy_token: H160::from_low_u64_be(2),
            buy_amount: U256::from(0),
            sell_amount: U256::from(0),
            fee_amount: U256::from(1),
            signature: Signature::Eip712(EcdsaSignature::non_zero()),
            ..Default::default()
        };
        let result = validator
            .validate_and_construct_order(order, &Default::default(), Default::default(), None)
            .await;
        assert!(matches!(result, Err(ValidationError::ZeroAmount)));
    }

    #[tokio::test]
    async fn post_zero_fee_limit_orders_disabled() {
        let mut order_quoter = MockOrderQuoting::new();
        let mut bad_token_detector = MockBadTokenDetecting::new();
        let mut balance_fetcher = MockBalanceFetching::new();
        order_quoter.expect_find_quote().returning(|_, _, _| {
            Ok(Quote {
                fee_amount: U256::from(1),
                ..Default::default()
            })
        });
        bad_token_detector
            .expect_detect()
            .returning(|_| Ok(TokenQuality::Good));
        balance_fetcher
            .expect_can_transfer()
            .returning(|_, _, _, _| Ok(()));
        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter.expect_count().returning(|_| Ok(0u64));
        let validator = OrderValidator::new(
            dummy_contract!(WETH9, [0xef; 20]),
            hashset!(),
            hashset!(),
            OrderValidPeriodConfiguration::any(),
            SignatureConfiguration::all(),
            Arc::new(bad_token_detector),
            Arc::new(order_quoter),
            Arc::new(balance_fetcher),
            Arc::new(MockSignatureValidating::new()),
            Arc::new(limit_order_counter),
            0,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        );
        let order = OrderCreation {
            valid_to: time::now_in_epoch_seconds() + 2,
            sell_token: H160::from_low_u64_be(1),
            buy_token: H160::from_low_u64_be(2),
            buy_amount: U256::from(1),
            sell_amount: U256::from(1),
            fee_amount: U256::zero(),
            signature: Signature::Eip712(EcdsaSignature::non_zero()),
            ..Default::default()
        };
        let result = validator
            .validate_and_construct_order(order, &Default::default(), Default::default(), None)
            .await;
        assert!(
            matches!(
                result,
                Err(ValidationError::Partial(
                    PartialValidationError::UnsupportedOrderType
                ))
            ),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn post_out_of_market_orders_when_limit_orders_disabled() {
        let expected_buy_amount = U256::from(100);

        let mut order_quoter = MockOrderQuoting::new();
        let mut bad_token_detector = MockBadTokenDetecting::new();
        let mut balance_fetcher = MockBalanceFetching::new();
        order_quoter.expect_find_quote().returning(move |_, _, _| {
            Ok(Quote {
                buy_amount: expected_buy_amount,
                sell_amount: U256::from(1),
                fee_amount: U256::from(1),
                ..Default::default()
            })
        });
        bad_token_detector
            .expect_detect()
            .returning(|_| Ok(TokenQuality::Good));
        balance_fetcher
            .expect_can_transfer()
            .returning(|_, _, _, _| Ok(()));
        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter.expect_count().returning(|_| Ok(0u64));
        let validator = OrderValidator::new(
            dummy_contract!(WETH9, [0xef; 20]),
            hashset!(),
            hashset!(),
            OrderValidPeriodConfiguration::any(),
            SignatureConfiguration::all(),
            Arc::new(bad_token_detector),
            Arc::new(order_quoter),
            Arc::new(balance_fetcher),
            Arc::new(MockSignatureValidating::new()),
            Arc::new(limit_order_counter),
            0,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        );
        let order = OrderCreation {
            valid_to: time::now_in_epoch_seconds() + 2,
            sell_token: H160::from_low_u64_be(1),
            buy_token: H160::from_low_u64_be(2),
            buy_amount: expected_buy_amount + 1, // buy more than expected
            sell_amount: U256::from(1),
            fee_amount: U256::from(1),
            kind: OrderKind::Sell,
            signature: Signature::Eip712(EcdsaSignature::non_zero()),
            ..Default::default()
        };
        let (order, quote) = validator
            .validate_and_construct_order(order, &Default::default(), Default::default(), None)
            .await
            .unwrap();

        // Out-of-price orders are intentionally marked as liquidity
        // orders!
        assert_eq!(order.metadata.class, OrderClass::Liquidity);
        assert!(quote.is_some());
    }

    #[tokio::test]
    async fn post_validate_err_wrong_owner() {
        let mut order_quoter = MockOrderQuoting::new();
        let mut bad_token_detector = MockBadTokenDetecting::new();
        let mut balance_fetcher = MockBalanceFetching::new();
        order_quoter
            .expect_find_quote()
            .returning(|_, _, _| Ok(Default::default()));
        bad_token_detector
            .expect_detect()
            .returning(|_| Ok(TokenQuality::Good));
        balance_fetcher
            .expect_can_transfer()
            .returning(|_, _, _, _| Ok(()));
        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter.expect_count().returning(|_| Ok(0u64));
        let validator = OrderValidator::new(
            dummy_contract!(WETH9, [0xef; 20]),
            hashset!(),
            hashset!(),
            OrderValidPeriodConfiguration::any(),
            SignatureConfiguration::all(),
            Arc::new(bad_token_detector),
            Arc::new(order_quoter),
            Arc::new(balance_fetcher),
            Arc::new(MockSignatureValidating::new()),
            Arc::new(limit_order_counter),
            0,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        );
        let order = OrderCreation {
            valid_to: time::now_in_epoch_seconds() + 2,
            sell_token: H160::from_low_u64_be(1),
            buy_token: H160::from_low_u64_be(2),
            buy_amount: U256::from(1),
            sell_amount: U256::from(1),
            fee_amount: U256::from(1),
            from: Some(Default::default()),
            signature: Signature::Eip712(EcdsaSignature::non_zero()),
            ..Default::default()
        };
        let result = validator
            .validate_and_construct_order(order, &Default::default(), Default::default(), None)
            .await;
        assert!(matches!(result, Err(ValidationError::WrongOwner(_))));
    }

    #[tokio::test]
    async fn post_validate_err_getting_quote() {
        let mut order_quoter = MockOrderQuoting::new();
        let mut bad_token_detector = MockBadTokenDetecting::new();
        let mut balance_fetcher = MockBalanceFetching::new();
        order_quoter
            .expect_find_quote()
            .returning(|_, _, _| Err(FindQuoteError::Other(anyhow!("err"))));
        bad_token_detector
            .expect_detect()
            .returning(|_| Ok(TokenQuality::Good));
        balance_fetcher
            .expect_can_transfer()
            .returning(|_, _, _, _| Ok(()));
        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter.expect_count().returning(|_| Ok(0u64));
        let validator = OrderValidator::new(
            dummy_contract!(WETH9, [0xef; 20]),
            hashset!(),
            hashset!(),
            OrderValidPeriodConfiguration::any(),
            SignatureConfiguration::all(),
            Arc::new(bad_token_detector),
            Arc::new(order_quoter),
            Arc::new(balance_fetcher),
            Arc::new(MockSignatureValidating::new()),
            Arc::new(limit_order_counter),
            0,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        );
        let order = OrderCreation {
            valid_to: time::now_in_epoch_seconds() + 2,
            sell_token: H160::from_low_u64_be(1),
            buy_token: H160::from_low_u64_be(2),
            buy_amount: U256::from(1),
            sell_amount: U256::from(1),
            fee_amount: U256::from(1),
            signature: Signature::Eip712(EcdsaSignature::non_zero()),
            ..Default::default()
        };
        let result = validator
            .validate_and_construct_order(order, &Default::default(), Default::default(), None)
            .await;
        dbg!(&result);
        assert!(matches!(result, Err(ValidationError::Other(_))));
    }

    #[tokio::test]
    async fn post_validate_err_unsupported_token() {
        let mut order_quoter = MockOrderQuoting::new();
        let mut bad_token_detector = MockBadTokenDetecting::new();
        let mut balance_fetcher = MockBalanceFetching::new();
        order_quoter
            .expect_find_quote()
            .returning(|_, _, _| Ok(Default::default()));
        bad_token_detector.expect_detect().returning(|_| {
            Ok(TokenQuality::Bad {
                reason: Default::default(),
            })
        });
        balance_fetcher
            .expect_can_transfer()
            .returning(|_, _, _, _| Ok(()));
        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter.expect_count().returning(|_| Ok(0u64));
        let validator = OrderValidator::new(
            dummy_contract!(WETH9, [0xef; 20]),
            hashset!(),
            hashset!(),
            OrderValidPeriodConfiguration::any(),
            SignatureConfiguration::all(),
            Arc::new(bad_token_detector),
            Arc::new(order_quoter),
            Arc::new(balance_fetcher),
            Arc::new(MockSignatureValidating::new()),
            Arc::new(limit_order_counter),
            0,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        );
        let order = OrderCreation {
            valid_to: time::now_in_epoch_seconds() + 2,
            sell_token: H160::from_low_u64_be(1),
            buy_token: H160::from_low_u64_be(2),
            buy_amount: U256::from(1),
            sell_amount: U256::from(1),
            fee_amount: U256::from(1),
            signature: Signature::Eip712(EcdsaSignature::non_zero()),
            ..Default::default()
        };
        let result = validator
            .validate_and_construct_order(order, &Default::default(), Default::default(), None)
            .await;
        dbg!(&result);
        assert!(matches!(
            result,
            Err(ValidationError::Partial(
                PartialValidationError::UnsupportedToken { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn post_validate_err_sell_amount_overflow() {
        let mut order_quoter = MockOrderQuoting::new();
        let mut bad_token_detector = MockBadTokenDetecting::new();
        let mut balance_fetcher = MockBalanceFetching::new();
        order_quoter
            .expect_find_quote()
            .returning(|_, _, _| Ok(Default::default()));
        order_quoter.expect_store_quote().returning(Ok);
        bad_token_detector
            .expect_detect()
            .returning(|_| Ok(TokenQuality::Good));
        balance_fetcher
            .expect_can_transfer()
            .returning(|_, _, _, _| Ok(()));
        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter.expect_count().returning(|_| Ok(0u64));
        let validator = OrderValidator::new(
            dummy_contract!(WETH9, [0xef; 20]),
            hashset!(),
            hashset!(),
            OrderValidPeriodConfiguration::any(),
            SignatureConfiguration::all(),
            Arc::new(bad_token_detector),
            Arc::new(order_quoter),
            Arc::new(balance_fetcher),
            Arc::new(MockSignatureValidating::new()),
            Arc::new(limit_order_counter),
            0,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        );
        let order = OrderCreation {
            valid_to: time::now_in_epoch_seconds() + 2,
            sell_token: H160::from_low_u64_be(1),
            buy_token: H160::from_low_u64_be(2),
            buy_amount: U256::from(1),
            sell_amount: U256::MAX,
            fee_amount: U256::from(1),
            signature: Signature::Eip712(EcdsaSignature::non_zero()),
            ..Default::default()
        };
        let result = validator
            .validate_and_construct_order(order, &Default::default(), Default::default(), None)
            .await;
        dbg!(&result);
        assert!(matches!(result, Err(ValidationError::SellAmountOverflow)));
    }

    #[tokio::test]
    async fn post_validate_err_insufficient_balance() {
        let mut order_quoter = MockOrderQuoting::new();
        let mut bad_token_detector = MockBadTokenDetecting::new();
        let mut balance_fetcher = MockBalanceFetching::new();
        order_quoter
            .expect_find_quote()
            .returning(|_, _, _| Ok(Default::default()));
        bad_token_detector
            .expect_detect()
            .returning(|_| Ok(TokenQuality::Good));
        balance_fetcher
            .expect_can_transfer()
            .returning(|_, _, _, _| Err(TransferSimulationError::InsufficientBalance));
        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter.expect_count().returning(|_| Ok(0u64));
        let validator = OrderValidator::new(
            dummy_contract!(WETH9, [0xef; 20]),
            hashset!(),
            hashset!(),
            OrderValidPeriodConfiguration::any(),
            SignatureConfiguration::all(),
            Arc::new(bad_token_detector),
            Arc::new(order_quoter),
            Arc::new(balance_fetcher),
            Arc::new(MockSignatureValidating::new()),
            Arc::new(limit_order_counter),
            0,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        );
        let order = OrderCreation {
            valid_to: time::now_in_epoch_seconds() + 2,
            sell_token: H160::from_low_u64_be(1),
            buy_token: H160::from_low_u64_be(2),
            buy_amount: U256::from(1),
            sell_amount: U256::from(1),
            fee_amount: U256::from(1),
            signature: Signature::Eip712(EcdsaSignature::non_zero()),
            ..Default::default()
        };
        let result = validator
            .validate_and_construct_order(order, &Default::default(), Default::default(), None)
            .await;
        dbg!(&result);
        assert!(matches!(result, Err(ValidationError::InsufficientBalance)));
    }

    #[tokio::test]
    async fn post_validate_err_invalid_eip1271_signature() {
        let mut order_quoter = MockOrderQuoting::new();
        let mut bad_token_detector = MockBadTokenDetecting::new();
        let mut balance_fetcher = MockBalanceFetching::new();
        let mut signature_validator = MockSignatureValidating::new();
        order_quoter
            .expect_find_quote()
            .returning(|_, _, _| Ok(Default::default()));
        bad_token_detector
            .expect_detect()
            .returning(|_| Ok(TokenQuality::Good));
        balance_fetcher
            .expect_can_transfer()
            .returning(|_, _, _, _| Ok(()));
        signature_validator
            .expect_validate_signature_and_get_additional_gas()
            .returning(|_| Err(SignatureValidationError::Invalid));
        let mut limit_order_counter = MockLimitOrderCounting::new();
        limit_order_counter.expect_count().returning(|_| Ok(0u64));
        let validator = OrderValidator::new(
            dummy_contract!(WETH9, [0xef; 20]),
            hashset!(),
            hashset!(),
            OrderValidPeriodConfiguration::any(),
            SignatureConfiguration::all(),
            Arc::new(bad_token_detector),
            Arc::new(order_quoter),
            Arc::new(balance_fetcher),
            Arc::new(signature_validator),
            Arc::new(limit_order_counter),
            0,
            Arc::new(MockCodeFetching::new()),
            Default::default(),
        );

        let creation = OrderCreation {
            valid_to: time::now_in_epoch_seconds() + 2,
            sell_token: H160::from_low_u64_be(1),
            buy_token: H160::from_low_u64_be(2),
            buy_amount: U256::from(1),
            sell_amount: U256::from(1),
            fee_amount: U256::from(1),
            from: Some(H160([1; 20])),
            signature: Signature::Eip1271(vec![1, 2, 3]),
            ..Default::default()
        };
        let domain = DomainSeparator::default();

        assert!(matches!(
            validator
                .validate_and_construct_order(creation.clone(), &domain, Default::default(), None)
                .await
                .unwrap_err(),
            ValidationError::InvalidEip1271Signature(hash)
                if hash.0 == signature::hashed_eip712_message(&domain, &creation.data().hash_struct()),
        ));
    }

    #[test]
    fn allows_insufficient_allowance_and_balance_for_presign_orders() {
        fn assert_allows_failed_transfer(
            create_error: impl Fn() -> TransferSimulationError + Send + 'static,
            is_expected_error: impl Fn(ValidationError) -> bool,
        ) {
            let mut order_quoter = MockOrderQuoting::new();
            let mut bad_token_detector = MockBadTokenDetecting::new();
            let mut balance_fetcher = MockBalanceFetching::new();
            order_quoter
                .expect_find_quote()
                .returning(|_, _, _| Ok(Default::default()));
            bad_token_detector
                .expect_detect()
                .returning(|_| Ok(TokenQuality::Good));
            balance_fetcher
                .expect_can_transfer()
                .returning(move |_, _, _, _| Err(create_error()));
            let mut limit_order_counter = MockLimitOrderCounting::new();
            limit_order_counter.expect_count().returning(|_| Ok(0u64));
            let validator = OrderValidator::new(
                dummy_contract!(WETH9, [0xef; 20]),
                hashset!(),
                hashset!(),
                OrderValidPeriodConfiguration::any(),
                SignatureConfiguration::all(),
                Arc::new(bad_token_detector),
                Arc::new(order_quoter),
                Arc::new(balance_fetcher),
                Arc::new(MockSignatureValidating::new()),
                Arc::new(limit_order_counter),
                0,
                Arc::new(MockCodeFetching::new()),
                Default::default(),
            );

            let order = OrderCreation {
                valid_to: u32::MAX,
                sell_token: H160::from_low_u64_be(1),
                sell_amount: 1.into(),
                buy_token: H160::from_low_u64_be(2),
                buy_amount: 1.into(),
                fee_amount: 1.into(),
                ..Default::default()
            };

            for signing_scheme in [EcdsaSigningScheme::Eip712, EcdsaSigningScheme::EthSign] {
                let err = validator
                    .validate_and_construct_order(
                        order.clone().sign(
                            signing_scheme,
                            &Default::default(),
                            SecretKeyRef::new(&ONE_KEY),
                        ),
                        &Default::default(),
                        Default::default(),
                        None,
                    )
                    .now_or_never()
                    .unwrap()
                    .unwrap_err();
                assert!(is_expected_error(err));
            }

            let order = OrderCreation {
                signature: Signature::PreSign,
                from: Some(Default::default()),
                ..order
            };
            validator
                .validate_and_construct_order(order, &Default::default(), Default::default(), None)
                .now_or_never()
                .unwrap()
                .unwrap();
        }

        assert_allows_failed_transfer(
            || TransferSimulationError::InsufficientAllowance,
            |e| matches!(e, ValidationError::InsufficientAllowance),
        );
        assert_allows_failed_transfer(
            || TransferSimulationError::InsufficientBalance,
            |e| matches!(e, ValidationError::InsufficientBalance),
        );
    }

    #[tokio::test]
    async fn get_quote_find_by_id() {
        let mut order_quoter = MockOrderQuoting::new();
        let quote_search_parameters = QuoteSearchParameters {
            sell_token: H160([1; 20]),
            buy_token: H160([2; 20]),
            sell_amount: 3.into(),
            buy_amount: 4.into(),
            fee_amount: 6.into(),
            kind: OrderKind::Buy,
            verification: Some(Verification {
                from: H160([0xf0; 20]),
                ..Default::default()
            }),
        };
        let quote_data = Quote {
            fee_amount: 6.into(),
            ..Default::default()
        };
        let fee_amount = quote_data.fee_amount;
        let quote_id = Some(42);
        let quote_signing_scheme = QuoteSigningScheme::Eip1271 {
            onchain_order: true,
            verification_gas_limit: default_verification_gas_limit(),
        };
        order_quoter
            .expect_find_quote()
            .with(
                eq(quote_id),
                eq(quote_search_parameters.clone()),
                eq(quote_signing_scheme),
            )
            .returning(move |_, _, _| Ok(quote_data.clone()));

        let quote = get_quote_and_check_fee(
            &order_quoter,
            &quote_search_parameters,
            quote_id,
            fee_amount,
            quote_signing_scheme,
        )
        .await
        .unwrap();

        assert_eq!(
            quote,
            Quote {
                fee_amount: 6.into(),
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn get_quote_calculates_fresh_quote_when_not_found() {
        let verification = Some(Verification {
            from: H160([0xf0; 20]),
            ..Default::default()
        });

        let mut order_quoter = MockOrderQuoting::new();
        order_quoter
            .expect_find_quote()
            .with(eq(None), always(), eq(&QuoteSigningScheme::Eip712))
            .returning(|_, _, _| Err(FindQuoteError::NotFound(None)));
        let quote_search_parameters = QuoteSearchParameters {
            sell_token: H160([1; 20]),
            buy_token: H160([2; 20]),
            kind: OrderKind::Sell,
            verification: verification.clone(),
            ..Default::default()
        };
        let quote_data = Quote {
            fee_amount: 6.into(),
            ..Default::default()
        };
        let fee_amount = quote_data.fee_amount;
        order_quoter
            .expect_calculate_quote()
            .with(eq(QuoteParameters {
                sell_token: quote_search_parameters.sell_token,
                buy_token: quote_search_parameters.buy_token,
                side: OrderQuoteSide::Sell {
                    sell_amount: SellAmount::AfterFee {
                        value: quote_search_parameters.sell_amount,
                    },
                },
                verification,
                signing_scheme: QuoteSigningScheme::Eip712,
            }))
            .returning({
                let quote_data = quote_data.clone();
                move |_| Ok(quote_data.clone())
            });
        order_quoter
            .expect_store_quote()
            .with(eq(quote_data.clone()))
            .returning(|quote| {
                Ok(Quote {
                    id: Some(42),
                    ..quote
                })
            });

        let quote = get_quote_and_check_fee(
            &order_quoter,
            &quote_search_parameters,
            None,
            fee_amount,
            QuoteSigningScheme::Eip712,
        )
        .await
        .unwrap();

        assert_eq!(
            quote,
            Quote {
                id: Some(42),
                fee_amount: 6.into(),
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn get_quote_errors_when_not_found_by_id() {
        let quote_search_parameters = QuoteSearchParameters {
            ..Default::default()
        };

        let mut order_quoter = MockOrderQuoting::new();
        order_quoter
            .expect_find_quote()
            .returning(|_, _, _| Err(FindQuoteError::NotFound(Some(0))));

        let err = get_quote_and_check_fee(
            &order_quoter,
            &quote_search_parameters,
            Some(0),
            U256::zero(),
            QuoteSigningScheme::Eip712,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, ValidationError::QuoteNotFound));
    }

    #[tokio::test]
    async fn get_quote_errors_on_insufficient_fees() {
        let mut order_quoter = MockOrderQuoting::new();
        order_quoter.expect_find_quote().returning(|_, _, _| {
            Ok(Quote {
                fee_amount: 2.into(),
                ..Default::default()
            })
        });

        let err = get_quote_and_check_fee(
            &order_quoter,
            &Default::default(),
            Default::default(),
            U256::one(),
            QuoteSigningScheme::Eip712,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, ValidationError::InsufficientFee));
    }

    #[tokio::test]
    async fn get_quote_bubbles_errors() {
        macro_rules! assert_find_error_matches {
            ($find_err:expr, $validation_err:pat) => {{
                let mut order_quoter = MockOrderQuoting::new();
                order_quoter
                    .expect_find_quote()
                    .returning(|_, _, _| Err($find_err));
                let err = get_quote_and_check_fee(
                    &order_quoter,
                    &Default::default(),
                    Default::default(),
                    Default::default(),
                    QuoteSigningScheme::Eip712,
                )
                .await
                .unwrap_err();

                assert!(matches!(err, $validation_err));
            }};
        }

        assert_find_error_matches!(
            FindQuoteError::Expired(Utc::now()),
            ValidationError::InvalidQuote
        );
        assert_find_error_matches!(
            FindQuoteError::ParameterMismatch(Default::default()),
            ValidationError::InvalidQuote
        );

        macro_rules! assert_calc_error_matches {
            ($calc_err:expr, $validation_err:pat) => {{
                let mut order_quoter = MockOrderQuoting::new();
                order_quoter
                    .expect_find_quote()
                    .returning(|_, _, _| Err(FindQuoteError::NotFound(None)));
                order_quoter
                    .expect_calculate_quote()
                    .returning(|_| Err($calc_err));

                let err = get_quote_and_check_fee(
                    &order_quoter,
                    &Default::default(),
                    Default::default(),
                    U256::zero(),
                    QuoteSigningScheme::Eip712,
                )
                .await
                .unwrap_err();

                assert!(matches!(err, $validation_err));
            }};
        }

        assert_calc_error_matches!(
            CalculateQuoteError::SellAmountDoesNotCoverFee {
                fee_amount: Default::default()
            },
            ValidationError::Other(_)
        );
        assert_calc_error_matches!(
            CalculateQuoteError::Price(PriceEstimationError::UnsupportedToken {
                token: Default::default(),
                reason: Default::default()
            }),
            ValidationError::Partial(PartialValidationError::UnsupportedToken { .. })
        );
        assert_calc_error_matches!(
            CalculateQuoteError::Price(PriceEstimationError::ZeroAmount),
            ValidationError::ZeroAmount
        );
        assert_calc_error_matches!(
            CalculateQuoteError::Price(PriceEstimationError::NoLiquidity),
            ValidationError::PriceForQuote(_)
        );
        assert_calc_error_matches!(
            CalculateQuoteError::Price(PriceEstimationError::UnsupportedOrderType),
            ValidationError::PriceForQuote(_)
        );
        assert_calc_error_matches!(
            CalculateQuoteError::Price(PriceEstimationError::RateLimited),
            ValidationError::PriceForQuote(_)
        );
    }

    #[test]
    fn detects_market_orders() {
        let quote = Quote {
            sell_amount: 100.into(),
            buy_amount: 100.into(),
            ..Default::default()
        };

        // at market price
        assert!(!is_order_outside_market_price(
            &"100".into(),
            &"100".into(),
            &quote,
        ));
        // willing to buy less than market price
        assert!(!is_order_outside_market_price(
            &"100".into(),
            &"90".into(),
            &quote,
        ));
        // wanting to buy more than market price
        assert!(is_order_outside_market_price(
            &"100".into(),
            &"1000".into(),
            &quote
        ));
    }
}
