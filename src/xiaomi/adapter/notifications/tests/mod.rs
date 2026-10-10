pub(super) mod support;

use super::*;
use futures_lite::future::{block_on, poll_once};
use futures_util::{StreamExt, stream::FuturesUnordered};

#[test]
fn subscription_budget_releases_capacity_when_a_handshake_ends_or_is_dropped() {
    block_on(async {
        let budget = SubscriptionBudget::default();
        let changed = Event::new();
        let life = Lifetime::default();
        let mut permits = Vec::new();
        for _ in 0..SUBSCRIPTION_LIMIT {
            permits.push(budget.acquire(&changed, &life, &|| true).await.unwrap());
        }
        let mut waiting = std::pin::pin!(budget.acquire(&changed, &life, &|| true));
        assert!(poll_once(&mut waiting).await.is_none());
        drop(permits.pop());
        let permit = poll_once(&mut waiting).await.unwrap().unwrap();
        assert_eq!(budget.in_flight.get(), SUBSCRIPTION_LIMIT);
        drop(permit);
        drop(permits);
        assert_eq!(budget.in_flight.get(), 0);
    });
}

#[test]
fn batch_release_fills_every_available_slot_even_if_a_waiter_is_retired() {
    for retired in [false, true] {
        block_on(async {
            let budget = SubscriptionBudget::default();
            let changed = Event::new();
            let life = Lifetime::default();
            let mut permits = Vec::new();
            for _ in 0..SUBSCRIPTION_LIMIT {
                permits.push(budget.acquire(&changed, &life, &|| true).await.unwrap());
            }
            let current = (0..SUBSCRIPTION_LIMIT + usize::from(retired))
                .map(|_| Cell::new(true))
                .collect::<Vec<_>>();
            let valid = current
                .iter()
                .map(|value| move || value.get())
                .collect::<Vec<_>>();
            let mut waiting = valid
                .iter()
                .map(|valid| budget.acquire(&changed, &life, valid))
                .collect::<FuturesUnordered<_>>();
            assert!(poll_once(waiting.next()).await.is_none());
            if retired {
                // A generation can be retired after its last context wake was processed.
                current[0].set(false);
            }
            drop(permits);
            let mut acquired = Vec::new();
            for _ in 0..current.len() {
                let ready = poll_once(waiting.next())
                    .await
                    .expect("every available slot must wake another queued handshake")
                    .unwrap();
                if let Some(permit) = ready {
                    acquired.push(permit);
                }
            }
            assert_eq!(acquired.len(), SUBSCRIPTION_LIMIT);
            assert_eq!(budget.in_flight.get(), SUBSCRIPTION_LIMIT);
        });
    }
}

#[test]
fn waiting_subscription_cancels_without_releasing_an_active_handshake() {
    for ended in [false, true] {
        block_on(async {
            let budget = SubscriptionBudget::default();
            let changed = Event::new();
            let life = Lifetime::default();
            let current = Cell::new(true);
            let mut permits = Vec::new();
            for _ in 0..SUBSCRIPTION_LIMIT {
                permits.push(budget.acquire(&changed, &life, &|| true).await.unwrap());
            }
            let valid = || current.get();
            let mut waiting = std::pin::pin!(budget.acquire(&changed, &life, &valid));
            assert!(poll_once(&mut waiting).await.is_none());
            if ended {
                life.close();
            } else {
                current.set(false);
                changed.notify(usize::MAX);
            }
            assert!(poll_once(&mut waiting).await.unwrap().is_none());
            assert_eq!(budget.in_flight.get(), SUBSCRIPTION_LIMIT);
            drop(permits);
        });
    }
}

#[test]
fn available_subscription_capacity_does_not_admit_an_invalid_generation() {
    block_on(async {
        let budget = SubscriptionBudget::default();
        let changed = Event::new();
        let life = Lifetime::default();
        assert!(budget.acquire(&changed, &life, &|| false).await.is_none());
        assert_eq!(budget.in_flight.get(), 0);
    });
}
