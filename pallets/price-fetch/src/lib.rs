#![cfg_attr(not(feature = "std"), no_std)]

/// WIP - price fetch pallet
/// The target of this pallet is to get a reliable price point on request
///
/// The process of fetching and calculating the price starts by calling start_fetcher(symbol, duration)
/// where symbol is a ticker of fetched asset and duration is number of blocks for which we fetch the price.
/// This call should cost enough to cover the costs of all subsequent actions done by the pallet.
///
/// After start_fetcher is called, validators should fetch the price and post it as a transaction.
/// After the fetching period finishes, all of the prices are collected and evaluated.
///
/// If we see a large deviation in the prices, we can search for anomalies and slash misreporting validators.
/// The prices posted to runtime storage should be converted to non-floating point value to guarantee consensus on subsequent calculations.
///
/// We assume proof of stake environment, thus we can be sure this process is secured by validators stake.
///
use codec::{Decode, Encode};
use frame_support::{
	debug, decl_error, decl_event, decl_module, decl_storage, dispatch::DispatchResult, ensure, traits::Get,
};

use frame_system::{
	self as system, ensure_signed,
	offchain::{AppCrypto, CreateSignedTransaction, SendSignedTransaction, Signer},
};

use alt_serde::{Deserialize, Deserializer};

use primitives::Price;
use sp_core::crypto::KeyTypeId;
use sp_runtime::offchain::{http, Duration};
use sp_std::vec::Vec;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

pub type Symbol = Vec<u8>;

//TODO: this should be a param to start_fetcher(symbol, duration) function
const SYM: &[u8; 3] = b"ETH";
pub const SYMBOLS: [(&[u8], &[u8]); 1] = [(b"ETH", b"https://api.diadata.org/v1/quotation/ETH")];

// Specifying serde path as `alt_serde`
// ref: https://serde.rs/container-attrs.html#crate
#[serde(crate = "alt_serde")]
#[derive(Deserialize, Encode, Decode, Default, Clone, PartialEq, Debug)]
pub struct DiaPriceRecord {
	#[serde(rename(deserialize = "Price"))]
	#[serde(deserialize_with = "de_float_to_price")]
	price: Price,
	#[serde(deserialize_with = "de_string_to_bytes")]
	#[serde(rename(deserialize = "Time"))]
	time: Vec<u8>,
	#[serde(deserialize_with = "de_string_to_bytes")]
	#[serde(rename(deserialize = "Symbol"))]
	symbol: Symbol,
}

#[derive(Encode, Decode, Default, Clone, PartialEq, Debug)]
pub struct FetchedPrice<AccountId> {
	price: Price,
	time: Vec<u8>,
	symbol: Symbol,
	author: AccountId,
}

#[derive(Encode, Decode, Default, Clone, PartialEq, Debug)]
pub struct Fetcher<BlockNumber> {
	symbol: Symbol,
	url: Vec<u8>,
	end_fetching_at: BlockNumber,
}

pub fn de_string_to_bytes<'de, D>(de: D) -> Result<Vec<u8>, D::Error>
where
	D: Deserializer<'de>,
{
	let s: &str = Deserialize::deserialize(de)?;
	Ok(s.as_bytes().to_vec())
}

pub fn de_float_to_price<'de, D>(de: D) -> Result<Price, D::Error>
where
	D: Deserializer<'de>,
{
	let fp: f64 = Deserialize::deserialize(de)?;

	//TODO: CONST -> DECIMAL PLACES FOR PRICE.
	//		This will depend on the type used in our case sp_runtime::FixedU128
	//TODO: Make sure this doesn't overflow
	let int = (fp * (1_000_000_000_000_000_000_f64)) as u128;
	Ok(Price::from_inner(int))
}

pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"pocw");

pub mod crypto {
	use super::KEY_TYPE;
	use sp_core::sr25519::Signature as Sr25519Signature;
	use sp_runtime::app_crypto::{app_crypto, sr25519};
	use sp_runtime::{traits::Verify, MultiSignature, MultiSigner};

	app_crypto!(sr25519, KEY_TYPE);

	pub struct TestAuthId;
	impl frame_system::offchain::AppCrypto<MultiSigner, MultiSignature> for TestAuthId {
		type RuntimeAppPublic = Public;
		type GenericSignature = Sr25519Signature;
		type GenericPublic = sp_core::sr25519::Public;
	}

	//implemented for mock runtime in test
	impl frame_system::offchain::AppCrypto<<Sr25519Signature as Verify>::Signer, Sr25519Signature> for TestAuthId {
		type RuntimeAppPublic = Public;
		type GenericSignature = sp_core::sr25519::Signature;
		type GenericPublic = sp_core::sr25519::Public;
	}
}

/// This pallet's configuration trait
pub trait Trait: CreateSignedTransaction<Call<Self>> + pallet_timestamp::Trait + system::Trait {
	/// The identifier type for an offchain worker.
	type AuthorityId: AppCrypto<Self::Public, Self::Signature>;

	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as frame_system::Trait>::Event>;

	/// The overarching dispatch call type.
	type Call: From<Call<Self>>;

	/// Grace period between submitting prices. Submit price only every GracePeriod block
	type GracePeriod: Get<Self::BlockNumber>;
}

decl_storage! {
	trait Store for Module<T: Trait> as PriceFetch {
		///Map of currently running fetchers
		Fetchers get(fn fetcher): map hasher(identity) Vec<u8> => Fetcher<T::BlockNumber>;

		///Map of raw fetched_prices from oracle. Key is hash of symbol e.g hash('ETH')
		FetchedPrices get(fn fetched_prices): map hasher(identity) Vec<u8> => Vec<FetchedPrice<T::AccountId>>;

		///Map of aggregated prices
		AvgPrices get(fn avg_price): map hasher(identity) Vec<u8> => (T::Moment, Price, T::AccountId);
	}
}

decl_error! {
	pub enum Error for Module<T: Trait> {
		//Fetcher for required symbol is already running
		FetcherAlreadyExist,
		//start fetcher for unsupported symbol (currency/token, e.g ETH
		SymbolNotFound,

		FetcherNotFound,
	}
}

decl_event!(
	pub enum Event<T>
	where
		Moment = <T as pallet_timestamp::Trait>::Moment,
		AccountId = <T as frame_system::Trait>::AccountId,
		Price = Price,
		Symbol = Symbol,
	{
		//New fetcher was initialized
		NewFetcher(AccountId, Symbol, Moment),

		//New price point was saved from symbol
		NewPricePoint(AccountId, Symbol, Moment, Price),

		//New avg price was calculated and old fetcher was destroyed
		NewAvgPrice(AccountId, Symbol, Moment, Price),
	}
);

decl_module! {
	/// A public part of the pallet.
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {

		type Error = Error<T>;

		fn deposit_event() = default;

		///Start fetching price for 600 blocks
		//TODO: add fetched duration and symbol
		#[weight = 0]
		pub fn start_fetcher(origin) -> DispatchResult {
			let who = ensure_signed(origin)?;

			ensure!(!<Fetchers<T>>::contains_key(&SYM.to_vec()), Error::<T>::FetcherAlreadyExist);

			//TODO: duration should be param of function
			let end_at = <system::Module<T>>::block_number() + T::BlockNumber::from(600); //600 blocs is 1hour at 1 block/6s
			let url = match SYMBOLS.iter().find(|(s, _)| s == SYM) {
				Some (p) => Ok(p.1),
				None => Err(Error::<T>::SymbolNotFound)
			}?;

			let new_fetcher = Fetcher {
				symbol: SYM.to_vec(),
				end_fetching_at: end_at,
				url: url.to_vec()
			};
			<Fetchers<T>>::insert(SYM.to_vec(), new_fetcher);

			let now = <pallet_timestamp::Module<T>>::get();
			Self::deposit_event(RawEvent::NewFetcher(who, SYM.to_vec(), now));
			Ok(())
		}

		#[weight = 0]
		pub fn submit_new_price(origin, price_record: DiaPriceRecord) -> DispatchResult {
			let who = ensure_signed(origin)?;

			ensure!(<Fetchers<T>>::contains_key(&price_record.symbol), Error::<T>::FetcherNotFound);

			let new_price = FetchedPrice {
				price: price_record.price,
				time: price_record.time,
				symbol: price_record.symbol.clone(),
				author: who.clone()
			};

			Self::add_new_price_to_list(new_price);

			let now = <pallet_timestamp::Module<T>>::get();
			Self::deposit_event(RawEvent::NewPricePoint(who, price_record.symbol, now, price_record.price));

			Ok(())
		}

		#[weight = 0]
		pub fn submit_new_avg_price(origin, symbol: Symbol, avg_price:Price) -> DispatchResult {
			let who = ensure_signed(origin)?;

			let now = <pallet_timestamp::Module<T>>::get();
			<AvgPrices<T>>::insert(symbol.clone(), (now, avg_price, who.clone()));

			//delete finished fetcher and remove old data
			let _old_fetcher = <Fetchers<T>>::take(symbol.clone());
			let _old_prices = <FetchedPrices<T>>::take(symbol.clone());

			Self::deposit_event(RawEvent::NewAvgPrice(who, symbol, now, avg_price));

			Ok(())
		}

		fn offchain_worker(block_number: T::BlockNumber) {
			//NOTE: sp_io::offchain::is_validator()

			//NOTE: for higher amount of fetchers it would be better to use different storage structure to
			//minimize storage access
			<Fetchers<T>>::iter().for_each(|(_, f)| {

				//TASK I.: check fetchers that should end - calculate avg, submit price, and clear
				//storage
				if f.end_fetching_at <= block_number {
					if let Err(e) = Self::calc_and_submit_avg_price(f) {
						debug::error!("Error: {}", e);
					}
				} else if block_number % T::GracePeriod::get() == 0.into() {
					//TASK II.: Fetch and submit price
					if let Err(e) = Self::fetch_price_and_submit(f) {
						debug::error!("Error: {}", e);
					}
				}
			});
		}
	}
}

/// Most of the functions are moved outside of the `decl_module!` macro.
///
/// This greatly helps with error messages, as the ones inside the macro
/// can sometimes be hard to debug.
impl<T: Trait> Module<T> {
	fn add_new_price_to_list(price: FetchedPrice<T::AccountId>) {
		<FetchedPrices<T>>::mutate(price.symbol.clone(), |prices| {
			prices.push(price);
		});
	}

	//NOTE: consider move to onf_finalize
	fn calc_and_submit_avg_price(fetcher: Fetcher<T::BlockNumber>) -> Result<(), &'static str> {
		let signer = Signer::<T, T::AuthorityId>::all_accounts();
		if !signer.can_sign() {
			return Err("No local accounts available. Consider adding one via `author_insertKey` RPC.");
		}

		//TODO: add minimum samples count e.g avg price will be computed only if 100 samples was
		//submitted. Otherwise it will fail
		let price_points = <FetchedPrices<T>>::get(fetcher.symbol.clone());

		//TODO: clean up invalid prices
		let mut sum: Price = Price::from(0);
		let mut samples_count = Price::from(0);
		price_points.iter().for_each(|pp| {
			sum = sum + pp.price;
			samples_count = samples_count + Price::from(1);
		});

		let avg_price = sum / samples_count;

		let results = signer.send_signed_transaction(|_account| {
			// Received price is wrapped into a call to `submit_price` public function of this pallet.
			// This means that the transaction, when executed, will simply call that function passing
			// `price` as an argument.
			Call::submit_new_avg_price(fetcher.symbol.clone(), avg_price)
		});

		for (acc, res) in &results {
			match res {
				Ok(()) => debug::info!("New price submitted by [{:?}]", acc.id),
				Err(e) => debug::error!("[{:?}] Failed to submit transaction: {:?}", acc.id, e),
			}
		}

		Ok(())
	}

	fn fetch_price_and_submit(fetcher: Fetcher<T::BlockNumber>) -> Result<(), &'static str> {
		let signer = Signer::<T, T::AuthorityId>::all_accounts();
		if !signer.can_sign() {
			return Err("No local accounts available. Consider adding one via `author_insertKey` RPC.");
		}

		//NOTE: Blocking http request
		let fetched_price = Self::fetch_price(fetcher.url).map_err(|_| "Failed to fetch data")?;

		let results = signer.send_signed_transaction(|_account| {
			// Received price is wrapped into a call to `submit_price` public function of this pallet.
			// This means that the transaction, when executed, will simply call that function passing
			// `price` as an argument.
			Call::submit_new_price(fetched_price.clone())
		});

		for (acc, res) in &results {
			match res {
				Ok(()) => debug::info!("New price submitted by [{:?}]", acc.id),
				Err(e) => debug::error!("[{:?}] Failed to submit transaction: {:?}", acc.id, e),
			}
		}

		Ok(())
	}

	/// Fetch current price from url
	fn fetch_price(url: Vec<u8>) -> Result<DiaPriceRecord, http::Error> {
		// deadline to complete the external call.
		let deadline = sp_io::offchain::timestamp().add(Duration::from_millis(2_000));

		let request = http::Request::get(sp_std::str::from_utf8(&url).unwrap());
		let pending = request.deadline(deadline).send().map_err(|_| http::Error::IoError)?;

		let response = pending.try_wait(deadline).map_err(|_| http::Error::DeadlineReached)??;

		if response.code != 200 {
			debug::warn!("Unexpected status code: {}", response.code);
			return Err(http::Error::Unknown);
		}

		let body = response.body().collect::<Vec<u8>>();
		let body_str = sp_std::str::from_utf8(&body).map_err(|_| {
			debug::warn!("No UTF8 body");
			http::Error::Unknown
		})?;

		let price = match Self::parse_dia_res(body_str) {
			Some(price) => Ok(price),
			None => {
				debug::warn!("Unable to parse response: {:?}", body_str);
				Err(http::Error::Unknown)
			}
		}?;

		Ok(price)
	}

	/// Parse json response body received from dia request
	///
	/// Returns `None` when parsing failed or `Some(DiaPriceRecord)` when parsing is successful.
	fn parse_dia_res(body: &str) -> Option<DiaPriceRecord> {
		match serde_json::from_str(&body) {
			Ok(p) => Some(p),
			Err(_) => None,
		}
	}
}