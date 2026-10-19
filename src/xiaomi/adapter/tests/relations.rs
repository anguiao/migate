use super::super::relations::Relations;
use crate::configuration::Scope;
use crate::xiaomi::cloud::{
    CloudCatalogDevice, CloudDeviceCatalog, CloudGroupMembership, CloudGroupStatus,
};
use std::collections::BTreeSet;

fn scope() -> Scope {
    Scope {
        uid: "account".into(),
        home_id: "home".into(),
    }
}

fn home(dids: &[&str]) -> BTreeSet<String> {
    dids.iter().map(|did| (*did).to_string()).collect()
}

fn membership(group_did: &str, members: &[&str], physical: bool) -> CloudGroupMembership {
    CloudGroupMembership {
        group_did: group_did.into(),
        is_physical: physical,
        members: members.iter().map(|did| (*did).to_string()).collect(),
    }
}

fn device(did: &str, group: CloudGroupStatus) -> CloudCatalogDevice {
    CloudCatalogDevice {
        did: did.into(),
        name: Some(did.into()),
        model: "mijia.light.group3".into(),
        instance_type: Some(
            "urn:miot-spec-v2:device:light:0000A001:mijia-group3:3:0000C802".into(),
        ),
        online: true,
        group,
    }
}

fn catalog(devices: Vec<CloudCatalogDevice>) -> CloudDeviceCatalog {
    CloudDeviceCatalog { devices }
}

#[test]
fn confirmed_group_is_active_and_preserves_members() {
    let mut relations = Relations::default();
    let update = relations.apply(
        &catalog(vec![device(
            "group-1",
            CloudGroupStatus::Confirmed(membership("group-1", &["member-2", "member-3"], false)),
        )]),
        &scope(),
        &home(&["member-2", "member-3"]),
    );
    assert_eq!(update.active.len(), 1);
    assert!(update.withdrawn.is_empty());
    assert_eq!(update.active[0].record_did, "group-1");
    assert_eq!(
        update.active[0].membership.members,
        ["member-2", "member-3"]
    );
    assert!(!update.active[0].membership.is_physical);
}

#[test]
fn present_record_with_missing_membership_keeps_the_last_relation() {
    for status in [CloudGroupStatus::NotGroup, CloudGroupStatus::Failed] {
        let mut relations = Relations::default();
        relations.apply(
            &catalog(vec![device(
                "outer-1",
                CloudGroupStatus::Confirmed(membership("group-1", &["member-2"], true)),
            )]),
            &scope(),
            &home(&["member-2"]),
        );
        let update = relations.apply(
            &catalog(vec![device("outer-1", status)]),
            &scope(),
            &home(&["member-2"]),
        );
        assert_eq!(update.active.len(), 1, "last relation must be preserved");
        assert!(update.withdrawn.is_empty());
        assert_eq!(update.errors.len(), 1);
        assert_eq!(update.active[0].membership.group_did, "group-1");
        assert!(update.active[0].error.is_some());
    }
}

#[test]
fn confirmed_empty_withdraws_while_the_record_remains_reserved() {
    let mut relations = Relations::default();
    relations.apply(
        &catalog(vec![device(
            "group-1",
            CloudGroupStatus::Confirmed(membership("group-1", &["member-2"], false)),
        )]),
        &scope(),
        &home(&["member-2"]),
    );
    let update = relations.apply(
        &catalog(vec![device(
            "group-1",
            CloudGroupStatus::Confirmed(membership("group-1", &[], false)),
        )]),
        &scope(),
        &home(&["member-2"]),
    );
    assert!(update.active.is_empty());
    assert_eq!(update.withdrawn, ["group-1"]);
    assert!(update.errors.is_empty(), "empty is not a parse failure");
    // The reserved record is still associated with its outer DID, so a later
    // present-but-invalid record is not mistaken for an absent group.
    let update = relations.apply(
        &catalog(vec![device("group-1", CloudGroupStatus::Failed)]),
        &scope(),
        &home(&["member-2"]),
    );
    assert!(update.withdrawn.is_empty());
    assert!(update.active.is_empty());
}

#[test]
fn complete_directory_absence_deletes_a_known_group() {
    let mut relations = Relations::default();
    relations.apply(
        &catalog(vec![device(
            "group-1",
            CloudGroupStatus::Confirmed(membership("group-1", &["member-2"], true)),
        )]),
        &scope(),
        &home(&["member-2"]),
    );
    let update = relations.apply(&catalog(vec![]), &scope(), &home(&["member-2"]));
    assert_eq!(update.withdrawn, ["group-1"]);
    assert!(update.active.is_empty());
    assert!(update.errors.is_empty());
}

#[test]
fn out_of_home_members_are_inactive_and_authorship_did_may_differ() {
    let mut relations = Relations::default();
    let update = relations.apply(
        &catalog(vec![device(
            "outer-record",
            CloudGroupStatus::Confirmed(membership("authoritative-group", &["elsewhere"], false)),
        )]),
        &scope(),
        &home(&["member-2"]),
    );
    assert!(update.active.is_empty());
    assert_eq!(update.errors.len(), 1);
    assert_eq!(update.errors[0].0, "authoritative-group");
    // A later in-home confirmation under a different outer record reactivates
    // the same authoritative group identity without duplicating it.
    let update = relations.apply(
        &catalog(vec![device(
            "outer-record-2",
            CloudGroupStatus::Confirmed(membership("authoritative-group", &["member-2"], false)),
        )]),
        &scope(),
        &home(&["member-2"]),
    );
    assert_eq!(update.active.len(), 1);
    assert_eq!(update.active[0].record_did, "outer-record-2");
    assert!(update.withdrawn.is_empty());
}

#[test]
fn first_time_empty_relation_cannot_establish_scope() {
    let mut relations = Relations::default();
    let update = relations.apply(
        &catalog(vec![device(
            "group-1",
            CloudGroupStatus::Confirmed(membership("group-1", &[], false)),
        )]),
        &scope(),
        &home(&[]),
    );
    assert!(update.active.is_empty());
    assert!(update.withdrawn.is_empty());
    assert!(update.errors.is_empty());
    // No invented scope exists, so a later failed record cannot revive one.
    let update = relations.apply(
        &catalog(vec![device("group-1", CloudGroupStatus::Failed)]),
        &scope(),
        &home(&[]),
    );
    assert!(update.active.is_empty());
    assert!(update.errors.is_empty());
}

#[test]
fn nested_group_members_produce_a_diagnostic_without_hiding() {
    let mut relations = Relations::default();
    let outer = device(
        "outer",
        CloudGroupStatus::Confirmed(membership("outer", &["inner", "member-3"], false)),
    );
    let members = home(&["member-2", "member-3"]);
    // A failed read must still identify the retained relation as a group.
    for inner in [
        CloudGroupStatus::Confirmed(membership("inner", &["member-2"], false)),
        CloudGroupStatus::Failed,
    ] {
        let update = relations.apply(
            &catalog(vec![outer.clone(), device("inner-record", inner)]),
            &scope(),
            &members,
        );
        assert_eq!(update.active.len(), 1);
        assert_eq!(update.active[0].membership.group_did, "inner");
        assert!(update.errors.iter().any(|(did, reason)| {
            did == "outer" && reason == "nested group members are not supported"
        }));
    }
    // Once the complete directory removes the inner group, its old identity
    // must not keep rejecting an otherwise valid relation with an unknown member.
    let update = relations.apply(&catalog(vec![outer]), &scope(), &members);
    assert_eq!(update.active.len(), 1);
    assert_eq!(update.active[0].membership.group_did, "outer");
    assert_eq!(update.withdrawn, ["inner"]);
    assert!(update.errors.is_empty());
}

#[test]
fn home_switch_scopes_successful_confirmation_and_drops_old_home_retention() {
    let mut relations = Relations::default();
    relations.apply(
        &catalog(vec![device(
            "record-1",
            CloudGroupStatus::Confirmed(membership("group-1", &["member-2"], false)),
        )]),
        &scope(),
        &home(&["member-2"]),
    );
    let other = Scope {
        uid: "account".into(),
        home_id: "other".into(),
    };
    // A present record without a confirmable member list cannot keep an
    // old-home relation alive in the new home.
    let update = relations.apply(
        &catalog(vec![device("record-1", CloudGroupStatus::Failed)]),
        &other,
        &home(&["member-2"]),
    );
    assert!(update.active.is_empty());
    assert!(update.withdrawn.contains(&"group-1".to_string()));
    // A successful confirmation in the new home uses the current scope, so a
    // later native fetch is not discarded by a stale scope guard.
    let update = relations.apply(
        &catalog(vec![device(
            "record-1",
            CloudGroupStatus::Confirmed(membership("group-1", &["member-2"], false)),
        )]),
        &other,
        &home(&["member-2"]),
    );
    assert_eq!(update.active.len(), 1);
    assert_eq!(update.active[0].scope.home_id, "other");
}

#[test]
fn mixed_home_members_keep_the_full_confirmed_list_and_stay_active() {
    let mut relations = Relations::default();
    let update = relations.apply(
        &catalog(vec![device(
            "mixed-record",
            CloudGroupStatus::Confirmed(membership(
                "mixed-group",
                &["member-2", "elsewhere"],
                false,
            )),
        )]),
        &scope(),
        &home(&["member-2"]),
    );
    // The approved spec scopes a group when it contains at least one member of
    // the selected home; foreign or unknown members stay in the confirmed list.
    assert!(update.errors.is_empty(), "{:?}", update.errors);
    assert_eq!(update.active.len(), 1);
    assert_eq!(
        update.active[0].membership.members,
        vec!["member-2".to_string(), "elsewhere".to_string()]
    );
}
