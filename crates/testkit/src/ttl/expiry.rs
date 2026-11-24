use soroban_sdk::testutils::storage::{Instance as _, Persistent as _, Temporary as _};
use soroban_sdk::testutils::Ledger as _;
use soroban_sdk::{Address, Env, IntoVal, Val};

use crate::core::{TestEnv, TestkitError};

/// Values captured at the moment [`TestEnv::expire`] runs.
///
/// Every expiry helper records these so that failure messages can say exactly
/// which ledger the expiry was induced at and what network limit was used to
/// compute the advance — without the caller having to query the environment
/// themselves.
///
/// The struct is `#[non_exhaustive]` so that fields can be added in future
/// versions without breaking existing matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExpiryContext {
    /// Ledger sequence number immediately before the advance.
    pub sequence_before: u32,
    /// Ledger sequence number immediately after the advance
    /// (`sequence_before + max_entry_ttl + 1`).
    pub sequence_after: u32,
    /// The `max_entry_ttl` read from [`soroban_sdk::testutils::LedgerInfo`]
    /// at the time `expire` ran. The advance is always this value plus one,
    /// so any entry that was live when `expire` was called is now expired.
    pub max_entry_ttl: u32,
}

/// Which of Soroban's three storage kinds an entry lives in. TTL semantics
/// differ per kind — see [`TestEnv::ttl_of`] and [`TestEnv::expire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
    /// Cheapest, expires soonest, and once gone is gone — used for data
    /// that's fine to lose (e.g. rate-limit counters, session state).
    Temporary,
    /// Long-lived, rent-paying storage. Expired entries can be restored by
    /// re-writing them; reading an expired entry panics.
    Persistent,
    /// The contract's own instance data (its "self" storage) plus its
    /// code. There is exactly one instance entry per contract, so
    /// [`TestEnv::ttl_of`] ignores the `key` parameter for this kind.
    Instance,
}

impl TestEnv {
    /// The current TTL of a storage entry, in ledgers.
    ///
    /// Ignores `key` for [`StorageKind::Instance`], which has one TTL per
    /// contract rather than one per key.
    ///
    /// # Panics
    ///
    /// Panics if the entry does not exist, or — for
    /// [`StorageKind::Persistent`] and [`StorageKind::Temporary`] — has
    /// already expired. This matches `soroban_sdk::testutils`' own
    /// `get_ttl` methods, which this is built on.
    ///
    /// # Example
    ///
    /// ```
    /// use soroban_testkit::core::TestEnv;
    /// use soroban_testkit::ttl::StorageKind;
    /// use soroban_sdk::{contract, contractimpl, symbol_short, Env, Symbol};
    ///
    /// #[contract]
    /// struct Store;
    ///
    /// #[contractimpl]
    /// impl Store {
    ///     pub fn set(env: Env, key: Symbol, value: i128) {
    ///         env.storage().persistent().set(&key, &value);
    ///     }
    /// }
    ///
    /// # fn main() {
    /// let env = TestEnv::new();
    /// let id = env.env().register(Store, ());
    /// StoreClient::new(env.env(), &id).set(&symbol_short!("k"), &1);
    ///
    /// let ttl = env.ttl_of(&id, StorageKind::Persistent, symbol_short!("k"));
    /// assert!(ttl > 0);
    /// # }
    /// ```
    pub fn ttl_of<K: IntoVal<Env, Val>>(
        &self,
        contract: &Address,
        kind: StorageKind,
        key: K,
    ) -> u32 {
        let key_val = key.into_val(self.env());
        self.ttl_of_val(contract, kind, &key_val)
    }

    /// Advance the ledger far enough that every entry — regardless of
    /// `kind` — expires, and return an [`ExpiryContext`] describing the
    /// values used.
    ///
    /// `kind` is accepted for symmetry with the rest of this module and to
    /// document intent at the call site, but does not change the amount
    /// advanced: from outside a contract, there is no way to know how much
    /// TTL headroom a specific entry has without calling
    /// [`TestEnv::ttl_of`] on it individually, and an entry of any kind
    /// may have been extended up to the network's `max_entry_ttl`. So this
    /// reads `max_entry_ttl` from the environment's current ledger info at
    /// runtime (never hardcoded — see [`crate::ledger::LEDGER_CLOSE_TIME_SECS`] for the
    /// same reasoning applied to ledger close time) and advances one
    /// ledger past it, which guarantees expiry for every kind at once.
    ///
    /// The returned [`ExpiryContext`] captures the sequence numbers before
    /// and after the advance, plus the `max_entry_ttl` that determined the
    /// step size. Callers that use `expire` as a building block (such as
    /// [`TestEnv::assert_survives_expiry`]) embed these values in any
    /// subsequent failure messages so that the diagnostics show exactly
    /// which ledger the expiry was induced at.
    ///
    /// # Example
    ///
    /// ```
    /// use soroban_testkit::core::TestEnv;
    /// use soroban_testkit::ttl::StorageKind;
    ///
    /// let env = TestEnv::new();
    /// let ctx = env.expire(StorageKind::Persistent);
    /// assert_eq!(ctx.sequence_after, ctx.sequence_before + ctx.max_entry_ttl + 1);
    /// ```
    pub fn expire(&self, kind: StorageKind) -> ExpiryContext {
        let _ = kind;
        let info = self.env().ledger().get();
        let sequence_before = info.sequence_number;
        let max_entry_ttl = info.max_entry_ttl;
        let advance = max_entry_ttl.saturating_add(1);
        self.advance_ledgers(advance);
        ExpiryContext {
            sequence_before,
            sequence_after: sequence_before.saturating_add(advance),
            max_entry_ttl,
        }
    }

    /// Assert that running `f` extends the TTL of the entry at `contract`/
    /// `kind`/`key` beyond what it was before `f` ran.
    ///
    /// The entry must already exist and have a readable TTL before `f` is
    /// called. If it does not, this helper panics with a
    /// [`TestkitError::Misuse`] that names the missing entry — a setup error
    /// in the test rather than a contract bug.
    ///
    /// # Panics
    ///
    /// - [`TestkitError::Misuse`] if the entry is absent or has no readable
    ///   TTL before `f` runs (the test did not create the entry first).
    /// - [`TestkitError::AssertionFailed`] showing the before and after TTLs
    ///   if `f` did not increase the TTL.
    ///
    /// # Example
    ///
    /// ```
    /// use soroban_testkit::core::TestEnv;
    /// use soroban_testkit::ttl::StorageKind;
    /// use soroban_sdk::{contract, contractimpl, symbol_short, Env, Symbol};
    ///
    /// #[contract]
    /// struct Store;
    ///
    /// #[contractimpl]
    /// impl Store {
    ///     pub fn set(env: Env, key: Symbol, value: i128) {
    ///         env.storage().persistent().set(&key, &value);
    ///     }
    ///     pub fn touch(env: Env, key: Symbol) {
    ///         env.storage().persistent().extend_ttl(&key, 5_000, 10_000);
    ///     }
    /// }
    ///
    /// # fn main() {
    /// let env = TestEnv::new();
    /// let id = env.env().register(Store, ());
    /// let client = StoreClient::new(env.env(), &id);
    /// client.set(&symbol_short!("k"), &1);
    ///
    /// env.assert_bumps_ttl(&id, StorageKind::Persistent, symbol_short!("k"), || {
    ///     client.touch(&symbol_short!("k"));
    /// });
    /// # }
    /// ```
    pub fn assert_bumps_ttl<K: IntoVal<Env, Val>>(
        &self,
        contract: &Address,
        kind: StorageKind,
        key: K,
        f: impl FnOnce(),
    ) {
        let key_val = key.into_val(self.env());

        // Read the TTL before running `f`. If this panics the entry was not
        // set up before the helper was called — that is a test-setup error,
        // not a contract bug, so report it as Misuse rather than letting the
        // raw SDK panic propagate.
        let before = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.ttl_of_val(contract, kind, &key_val)
        }))
        .unwrap_or_else(|_| {
            panic!(
                "{}",
                TestkitError::Misuse(format!(
                    "assert_bumps_ttl called on a {kind:?} entry that does not exist or has \
                     no readable TTL — write the entry before calling assert_bumps_ttl"
                ))
            )
        });

        f();

        let after = self.ttl_of_val(contract, kind, &key_val);
        if after <= before {
            panic!(
                "{}",
                TestkitError::AssertionFailed(format!(
                    "expected the call to extend the {kind:?} TTL beyond {before}, but it was {after} afterward"
                ))
            );
        }
    }

    /// Assert that `f` completes without panicking after every entry of
    /// `kind` has expired (via [`TestEnv::expire`]) — i.e. that the
    /// contract handles an expired/missing entry gracefully instead of
    /// trapping on it.
    ///
    /// # Panics
    ///
    /// Panics with a [`TestkitError::AssertionFailed`] naming the
    /// underlying panic and the ledger state used to induce the expiry
    /// (sequence before/after and `max_entry_ttl`) if `f` panics.
    ///
    /// # Example
    ///
    /// ```
    /// use soroban_testkit::core::TestEnv;
    /// use soroban_testkit::ttl::StorageKind;
    ///
    /// let env = TestEnv::new();
    /// env.assert_survives_expiry(StorageKind::Temporary, || {
    ///     // A closure that never touches the expired entry trivially survives.
    /// });
    /// ```
    pub fn assert_survives_expiry(&self, kind: StorageKind, f: impl FnOnce()) {
        let ctx = self.expire(kind);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        if let Err(payload) = result {
            panic!(
                "{}",
                TestkitError::AssertionFailed(format!(
                    "expected the contract to survive {kind:?} storage expiry gracefully, \
                     but it panicked: {} \
                     (ledger sequence advanced from {} to {} using max_entry_ttl={})",
                    crate::internal::panic_message(payload.as_ref()),
                    ctx.sequence_before,
                    ctx.sequence_after,
                    ctx.max_entry_ttl,
                ))
            );
        }
    }

    /// Assert that `f` completes without panicking after a persistent entry
    /// has expired **and** that the entry is live (has a readable TTL) once
    /// `f` returns.
    ///
    /// This covers the Soroban archival model's restoration path: a
    /// persistent entry whose TTL reaches zero is archived rather than
    /// destroyed, but reading it panics until the entry is re-written.
    /// The network-level mechanism is a *restore-footprint* transaction;
    /// the equivalent in a network-free test environment is for the
    /// contract to re-write the entry when it detects its absence (via
    /// `has()` or a defaulting `get()`).
    ///
    /// Two things are verified:
    ///
    /// 1. `f` does not panic — the contract handles the archived-entry
    ///    case gracefully.
    /// 2. After `f` returns, `contract`/`key` has a positive TTL — the
    ///    contract actually wrote the entry back rather than merely
    ///    tolerating its absence.
    ///
    /// # Panics
    ///
    /// - [`TestkitError::AssertionFailed`] if `f` panics after expiry
    ///   (the contract did not handle the missing-entry case).
    /// - [`TestkitError::AssertionFailed`] if the entry still has no
    ///   readable TTL after `f` returns (the contract survived but did
    ///   not restore the entry).
    ///
    /// # Example
    ///
    /// ```
    /// use soroban_testkit::core::TestEnv;
    /// use soroban_testkit::ttl::StorageKind;
    /// use soroban_sdk::{contract, contractimpl, symbol_short, Env, Symbol};
    ///
    /// #[contract]
    /// struct Store;
    ///
    /// #[contractimpl]
    /// impl Store {
    ///     pub fn set(env: Env, key: Symbol, value: i128) {
    ///         env.storage().persistent().set(&key, &value);
    ///     }
    ///     /// Correct restoration path: re-writes the entry if archived.
    ///     pub fn restore(env: Env, key: Symbol) -> i128 {
    ///         if !env.storage().persistent().has(&key) {
    ///             env.storage().persistent().set(&key, &0i128);
    ///         }
    ///         env.storage().persistent().get(&key).unwrap_or(0)
    ///     }
    /// }
    ///
    /// # fn main() {
    /// let env = TestEnv::new();
    /// let id = env.env().register(Store, ());
    /// let client = StoreClient::new(env.env(), &id);
    /// client.set(&symbol_short!("k"), &42i128);
    ///
    /// env.assert_restores_expired_persistent(
    ///     &id,
    ///     symbol_short!("k"),
    ///     || { client.restore(&symbol_short!("k")); },
    /// );
    /// # }
    /// ```
    pub fn assert_restores_expired_persistent<K: IntoVal<Env, Val>>(
        &self,
        contract: &Address,
        key: K,
        f: impl FnOnce(),
    ) {
        let key_val = key.into_val(self.env());
        let ctx = self.expire(StorageKind::Persistent);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        if let Err(payload) = result {
            panic!(
                "{}",
                TestkitError::AssertionFailed(format!(
                    "expected the contract to handle an expired Persistent entry and restore it, \
                     but it panicked: {} \
                     (ledger sequence advanced from {} to {} using max_entry_ttl={})",
                    crate::internal::panic_message(payload.as_ref()),
                    ctx.sequence_before,
                    ctx.sequence_after,
                    ctx.max_entry_ttl,
                ))
            );
        }

        // Verify the entry was actually restored: get_ttl panics if the
        // entry is still absent, so we catch that and report it clearly.
        let restored = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.ttl_of_val(contract, StorageKind::Persistent, &key_val)
        }));
        match restored {
            Ok(ttl) if ttl > 0 => {}
            Ok(ttl) => panic!(
                "{}",
                TestkitError::AssertionFailed(format!(
                    "contract survived expiry but the Persistent entry has TTL {ttl} after \
                     restoration — the entry must be written back with a positive TTL \
                     (ledger sequence advanced from {} to {} using max_entry_ttl={})",
                    ctx.sequence_before,
                    ctx.sequence_after,
                    ctx.max_entry_ttl,
                ))
            ),
            Err(_) => panic!(
                "{}",
                TestkitError::AssertionFailed(format!(
                    "contract survived expiry but the Persistent entry still has no readable TTL \
                     — the entry was not written back \
                     (ledger sequence advanced from {} to {} using max_entry_ttl={})",
                    ctx.sequence_before,
                    ctx.sequence_after,
                    ctx.max_entry_ttl,
                ))
            ),
        }
    }

    fn ttl_of_val(&self, contract: &Address, kind: StorageKind, key_val: &Val) -> u32 {
        self.env().as_contract(contract, || match kind {
            StorageKind::Temporary => self.env().storage().temporary().get_ttl(key_val),
            StorageKind::Persistent => self.env().storage().persistent().get_ttl(key_val),
            StorageKind::Instance => self.env().storage().instance().get_ttl(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vault::{DataKey, Vault, VaultClient};

    #[test]
    fn ttl_of_decreases_as_ledgers_advance() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_record(&1);

        let before = env.ttl_of(&id, StorageKind::Persistent, DataKey::Record);
        env.advance_ledgers(10);
        let after = env.ttl_of(&id, StorageKind::Persistent, DataKey::Record);

        assert!(after < before, "expected {after} < {before}");
        assert_eq!(before - after, 10);
    }

    // --- expire() returns a correctly populated ExpiryContext --------------

    #[test]
    fn expire_returns_context_with_correct_sequence_numbers() {
        let env = TestEnv::new();
        let seq_before = env.sequence();
        let ctx = env.expire(StorageKind::Persistent);

        assert_eq!(ctx.sequence_before, seq_before);
        assert_eq!(ctx.sequence_after, env.sequence());
        assert_eq!(
            ctx.sequence_after,
            ctx.sequence_before + ctx.max_entry_ttl + 1
        );
    }

    #[test]
    fn expire_context_max_entry_ttl_matches_ledger_info() {
        let env = TestEnv::new();
        // Read max_entry_ttl before the advance so we can verify the context.
        let max_entry_ttl = env.env().ledger().get().max_entry_ttl;
        let ctx = env.expire(StorageKind::Temporary);

        assert_eq!(ctx.max_entry_ttl, max_entry_ttl);
    }

    #[test]
    fn expire_context_sequence_before_is_the_sequence_at_call_time() {
        let env = TestEnv::new();
        env.advance_ledgers(7);
        let seq_at_call = env.sequence();

        let ctx = env.expire(StorageKind::Instance);

        assert_eq!(ctx.sequence_before, seq_at_call);
        assert!(ctx.sequence_after > ctx.sequence_before);
    }

    // --- assert_bumps_ttl --------------------------------------------------

    #[test]
    #[should_panic(expected = "expected the call to extend the Persistent TTL")]
    fn assert_bumps_ttl_fails_on_a_contract_that_reads_without_bumping() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_record(&1);

        env.assert_bumps_ttl(&id, StorageKind::Persistent, DataKey::Record, || {
            client.touch_record();
        });
    }

    #[test]
    fn assert_bumps_ttl_passes_on_the_fixed_contract() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_record(&1);

        env.assert_bumps_ttl(&id, StorageKind::Persistent, DataKey::Record, || {
            client.touch_record_checked();
        });
    }

    /// Calling assert_bumps_ttl before the entry exists must produce a clear
    /// Misuse error, not a raw SDK panic, for each storage kind.
    #[test]
    #[should_panic(expected = "assert_bumps_ttl called on a Persistent entry that does not exist")]
    fn assert_bumps_ttl_persistent_reports_misuse_when_entry_is_missing() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        // set_record is deliberately NOT called — the entry does not exist.
        env.assert_bumps_ttl(
            &id,
            StorageKind::Persistent,
            DataKey::Record,
            || { /* no-op */ },
        );
    }

    #[test]
    #[should_panic(expected = "assert_bumps_ttl called on a Temporary entry that does not exist")]
    fn assert_bumps_ttl_temporary_reports_misuse_when_entry_is_missing() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        // set_temp is deliberately NOT called.
        env.assert_bumps_ttl(
            &id,
            StorageKind::Temporary,
            DataKey::Temp,
            || { /* no-op */ },
        );
    }

    #[test]
    #[should_panic(expected = "assert_bumps_ttl called on a Instance entry that does not exist")]
    fn assert_bumps_ttl_instance_reports_misuse_when_entry_is_missing() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        // initialize is deliberately NOT called — instance storage is empty.
        env.assert_bumps_ttl(
            &id,
            StorageKind::Instance,
            DataKey::Owner,
            || { /* no-op */ },
        );
    }

    // --- assert_survives_expiry --------------------------------------------

    #[test]
    #[should_panic(expected = "expected the contract to survive")]
    fn assert_survives_expiry_fails_on_a_contract_that_panics_on_missing_entry() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_temp(&42);

        env.assert_survives_expiry(StorageKind::Temporary, || {
            client.read_temp_unchecked();
        });
    }

    #[test]
    fn assert_survives_expiry_passes_on_the_fixed_contract() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_temp(&42);

        env.assert_survives_expiry(StorageKind::Temporary, || {
            assert_eq!(client.read_temp_checked(), 0);
        });
    }

    /// The failure message must include the ledger context so the developer
    /// can see exactly what sequence advance was used.
    #[test]
    fn assert_survives_expiry_failure_message_includes_ledger_context() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_temp(&42);

        // Capture the values expire() will use, before it runs.
        let seq_before = env.sequence();
        let max_entry_ttl = env.env().ledger().get().max_entry_ttl;
        let seq_after = seq_before + max_entry_ttl + 1;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            env.assert_survives_expiry(StorageKind::Temporary, || {
                client.read_temp_unchecked();
            });
        }));

        let msg = crate::internal::panic_message(result.unwrap_err().as_ref());
        assert!(
            msg.contains(&format!(
                "ledger sequence advanced from {seq_before} to {seq_after} \
                 using max_entry_ttl={max_entry_ttl}"
            )),
            "expected context in failure message, got: {msg}"
        );
    }

    // --- Temporary storage: full helper coverage ---------------------------

    #[test]
    fn ttl_of_temporary_decreases_as_ledgers_advance() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_temp(&7);

        let before = env.ttl_of(&id, StorageKind::Temporary, DataKey::Temp);
        env.advance_ledgers(10);
        let after = env.ttl_of(&id, StorageKind::Temporary, DataKey::Temp);

        assert!(after < before, "expected {after} < {before}");
        assert_eq!(before - after, 10);
    }

    #[test]
    fn expire_temporary_advances_past_the_entry_ttl() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_temp(&7);

        let ctx = env.expire(StorageKind::Temporary);

        // After expiry the entry must be gone — ttl_of should panic because
        // get_ttl panics on a missing or expired temporary entry.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            env.ttl_of(&id, StorageKind::Temporary, DataKey::Temp)
        }));
        assert!(
            result.is_err(),
            "expected ttl_of to panic after temporary expiry (seq {} → {})",
            ctx.sequence_before,
            ctx.sequence_after
        );
    }

    #[test]
    #[should_panic(expected = "expected the call to extend the Temporary TTL")]
    fn assert_bumps_ttl_temporary_fails_when_contract_reads_without_extending() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_temp(&7);

        env.assert_bumps_ttl(&id, StorageKind::Temporary, DataKey::Temp, || {
            client.touch_temp();
        });
    }

    #[test]
    fn assert_bumps_ttl_temporary_passes_when_contract_extends_ttl() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_temp(&7);

        env.assert_bumps_ttl(&id, StorageKind::Temporary, DataKey::Temp, || {
            client.touch_temp_checked();
        });
    }

    #[test]
    #[should_panic(expected = "expected the contract to survive Temporary")]
    fn assert_survives_expiry_temporary_fails_when_contract_panics_on_missing() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_temp(&7);

        env.assert_survives_expiry(StorageKind::Temporary, || {
            client.read_temp_unchecked();
        });
    }

    #[test]
    fn assert_survives_expiry_temporary_passes_when_contract_handles_missing_gracefully() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_temp(&7);

        env.assert_survives_expiry(StorageKind::Temporary, || {
            assert_eq!(client.read_temp_checked(), 0);
        });
    }

    // --- Instance storage: full helper coverage ----------------------------

    #[test]
    fn ttl_of_instance_decreases_as_ledgers_advance() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        // initialize writes to instance storage, giving it a live TTL.
        let owner = env.address();
        VaultClient::new(env.env(), &id).initialize(&owner);

        let before = env.ttl_of(&id, StorageKind::Instance, DataKey::Owner);
        env.advance_ledgers(10);
        let after = env.ttl_of(&id, StorageKind::Instance, DataKey::Owner);

        assert!(after < before, "expected {after} < {before}");
        assert_eq!(before - after, 10);
    }

    #[test]
    fn expire_instance_advances_past_the_instance_ttl() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let owner = env.address();
        VaultClient::new(env.env(), &id).initialize(&owner);

        let ctx = env.expire(StorageKind::Instance);

        // After expiry get_ttl should panic because the instance TTL is gone.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            env.ttl_of(&id, StorageKind::Instance, DataKey::Owner)
        }));
        assert!(
            result.is_err(),
            "expected ttl_of to panic after instance expiry (seq {} → {})",
            ctx.sequence_before,
            ctx.sequence_after
        );
    }

    #[test]
    #[should_panic(expected = "expected the call to extend the Instance TTL")]
    fn assert_bumps_ttl_instance_fails_when_contract_reads_without_extending() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let owner = env.address();
        let client = VaultClient::new(env.env(), &id);
        client.initialize(&owner);

        // touch_instance reads the balance but does not call extend_ttl.
        env.assert_bumps_ttl(&id, StorageKind::Instance, DataKey::Owner, || {
            client.touch_instance().unwrap();
        });
    }

    #[test]
    fn assert_bumps_ttl_instance_passes_when_contract_extends_ttl() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let owner = env.address();
        let client = VaultClient::new(env.env(), &id);
        client.initialize(&owner);

        env.assert_bumps_ttl(&id, StorageKind::Instance, DataKey::Owner, || {
            client.touch_instance_checked().unwrap();
        });
    }

    #[test]
    fn assert_survives_expiry_instance_passes_when_contract_handles_missing_gracefully() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let owner = env.address();
        let client = VaultClient::new(env.env(), &id);
        client.initialize(&owner);

        // balance() uses `get(...).ok_or(NotInitialized)` — it returns a
        // contract error (VaultError::NotInitialized) rather than panicking
        // when the instance storage is absent after expiry. The SDK client's
        // `try_balance()` wraps that in `Ok(Err(NotInitialized))`, so the
        // invocation itself succeeds and no panic is raised.
        env.assert_survives_expiry(StorageKind::Instance, || {
            let result = client.try_balance();
            // The outer Ok means the invocation did not trap; the inner Err
            // means the contract returned VaultError::NotInitialized.
            assert!(
                matches!(result, Ok(Err(_))),
                "expected Ok(Err(NotInitialized)) after instance expiry, got {result:?}"
            );
        });
    }

    // --- assert_restores_expired_persistent --------------------------------

    #[test]
    #[should_panic(expected = "expected the contract to handle an expired Persistent entry")]
    fn assert_restores_expired_persistent_fails_when_contract_panics_on_missing_entry() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_record(&99);

        // restore_record_broken does an unconditional unwrap — panics after expiry.
        env.assert_restores_expired_persistent(&id, DataKey::Record, || {
            client.restore_record_broken();
        });
    }

    #[test]
    fn assert_restores_expired_persistent_passes_when_contract_rewrites_missing_entry() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_record(&99);

        // restore_record_checked uses has() to detect the archived entry and
        // re-writes it before reading, so both conditions are satisfied.
        env.assert_restores_expired_persistent(&id, DataKey::Record, || {
            let value = client.restore_record_checked();
            // Value is reset to 0 by the restoration path — the original 99
            // is gone once the entry is archived, matching the SDK model.
            assert_eq!(value, 0);
        });

        // After restoration the entry must have a positive TTL.
        let ttl = env.ttl_of(&id, StorageKind::Persistent, DataKey::Record);
        assert!(ttl > 0, "expected positive TTL after restoration, got {ttl}");
    }

    #[test]
    #[should_panic(expected = "Persistent entry still has no readable TTL")]
    fn assert_restores_expired_persistent_fails_when_contract_survives_but_does_not_rewrite() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_record(&99);

        // set_temp survives (returns a default) but never touches
        // DataKey::Record at all, so the persistent entry is not restored.
        env.assert_restores_expired_persistent(&id, DataKey::Record, || {
            client.set_temp(&0); // touches only Temp storage, not Record
        });
    }

    /// The "panicked" failure path must include ledger context.
    #[test]
    fn assert_restores_expired_persistent_panic_message_includes_ledger_context() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_record(&99);

        let seq_before = env.sequence();
        let max_entry_ttl = env.env().ledger().get().max_entry_ttl;
        let seq_after = seq_before + max_entry_ttl + 1;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            env.assert_restores_expired_persistent(&id, DataKey::Record, || {
                client.restore_record_broken();
            });
        }));

        let msg = crate::internal::panic_message(result.unwrap_err().as_ref());
        assert!(
            msg.contains(&format!(
                "ledger sequence advanced from {seq_before} to {seq_after} \
                 using max_entry_ttl={max_entry_ttl}"
            )),
            "expected context in failure message, got: {msg}"
        );
    }

    /// The "survived but did not rewrite" failure path must include ledger context.
    #[test]
    fn assert_restores_expired_persistent_no_rewrite_message_includes_ledger_context() {
        let env = TestEnv::new();
        let id = env.env().register(Vault, ());
        let client = VaultClient::new(env.env(), &id);
        client.set_record(&99);

        let seq_before = env.sequence();
        let max_entry_ttl = env.env().ledger().get().max_entry_ttl;
        let seq_after = seq_before + max_entry_ttl + 1;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            env.assert_restores_expired_persistent(&id, DataKey::Record, || {
                client.set_temp(&0);
            });
        }));

        let msg = crate::internal::panic_message(result.unwrap_err().as_ref());
        assert!(
            msg.contains(&format!(
                "ledger sequence advanced from {seq_before} to {seq_after} \
                 using max_entry_ttl={max_entry_ttl}"
            )),
            "expected context in failure message, got: {msg}"
        );
    }
}
