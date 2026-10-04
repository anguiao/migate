use super::super::{CentralError, wire};
use super::{
    CentralEvent, LIMIT,
    driver::{PublicationStage, SessionDriver, Subscription, SubscriptionKind},
    request::{
        RequestKind, Response, parse_response, property_topic, valid_identifier, validate_identity,
    },
};
use mqttbytes::{QoS, v5::*};
use serde_json::Value;
use std::time::Instant;

impl SessionDriver {
    pub(super) fn subscribe_baseline(&mut self, now: Instant) -> Result<(), CentralError> {
        let mut subscribe = Subscribe::new_many([
            SubscribeFilter::new(format!("{}/#", self.client_id), QoS::ExactlyOnce),
            SubscribeFilter::new("master/appMsg/devListChange".into(), QoS::ExactlyOnce),
        ]);
        subscribe.pkid = self.next_pkid()?;
        let deadline = now + self.timing.request;
        self.subscriptions.insert(
            subscribe.pkid,
            Subscription {
                request: None,
                deadline,
                count: 2,
                kind: SubscriptionKind::Baseline,
            },
        );
        self.enqueue(Packet::Subscribe(subscribe), deadline, None, None)?;
        Ok(())
    }
    pub(super) fn packet(
        &mut self,
        packet: Packet,
        on_event: &mut impl FnMut(CentralEvent),
    ) -> Result<(), CentralError> {
        match packet {
            Packet::SubAck(ack) => {
                if ack.pkid == 0 {
                    return Err(CentralError::Protocol);
                }
                let Some(subscription) = self.subscriptions.remove(&ack.pkid) else {
                    return Ok(());
                };
                if subscription.kind == SubscriptionKind::Unsubscribe {
                    return Err(CentralError::Protocol);
                }
                let error = if ack.return_codes.len() != subscription.count {
                    Some(CentralError::Protocol)
                } else {
                    ack.return_codes
                        .iter()
                        .find(|code| **code as u8 >= 128)
                        .map(|code| CentralError::Rejected(*code as i64))
                };
                if let Some(request) = subscription.request {
                    if let Some(error) = error {
                        self.finish(request, Err(error));
                    } else if Instant::now() >= request.deadline {
                        self.finish(request, Err(CentralError::Timeout));
                    } else {
                        if let RequestKind::Subscribe { did, binding } = &request.kind {
                            self.property_topics.insert(
                                property_topic(&self.client_id, did, *binding),
                                (did.clone(), *binding),
                            );
                        }
                        self.finish(request, Ok(Response::Unit));
                    }
                } else if subscription.kind == SubscriptionKind::Baseline {
                    if let Some(error) = error {
                        return Err(error);
                    }
                    self.ready = true;
                }
            }
            Packet::UnsubAck(ack) => {
                if ack.pkid == 0 {
                    return Err(CentralError::Protocol);
                }
                let Some(subscription) = self.subscriptions.remove(&ack.pkid) else {
                    return Ok(());
                };
                if subscription.kind != SubscriptionKind::Unsubscribe {
                    return Err(CentralError::Protocol);
                }
                let error = if ack.reasons.len() != subscription.count {
                    Some(CentralError::Protocol)
                } else {
                    ack.reasons
                        .iter()
                        .find(|reason| **reason as u8 >= 128)
                        .map(|reason| CentralError::Rejected(*reason as i64))
                };
                if let Some(request) = subscription.request {
                    let result = if let Some(error) = error {
                        Err(error)
                    } else if Instant::now() >= request.deadline {
                        Err(CentralError::Timeout)
                    } else {
                        Ok(Response::Unit)
                    };
                    self.finish(request, result);
                }
            }
            Packet::PubRec(ack) => {
                let Some(publication) = self.publications.get_mut(&ack.pkid) else {
                    return Err(CentralError::Protocol);
                };
                if ack.reason as u8 >= 128 {
                    let mid = publication.mid;
                    self.publications.remove(&ack.pkid);
                    if let Some(request) = self.pending.remove(&mid) {
                        self.finish(request, Err(CentralError::Rejected(ack.reason as i64)));
                    }
                } else {
                    publication.stage = PublicationStage::AwaitPubComp;
                    let deadline = publication.deadline;
                    self.enqueue(Packet::PubRel(PubRel::new(ack.pkid)), deadline, None, None)?;
                }
            }
            Packet::PubComp(ack) => {
                let Some(publication) = self.publications.remove(&ack.pkid) else {
                    return Err(CentralError::Protocol);
                };
                if publication.stage != PublicationStage::AwaitPubComp {
                    return Err(CentralError::Protocol);
                }
                if ack.reason != PubCompReason::Success
                    && let Some(request) = self.pending.remove(&publication.mid)
                {
                    self.finish(request, Err(CentralError::Rejected(ack.reason as i64)));
                }
            }
            Packet::Publish(publish) => {
                if publish.topic.is_empty()
                    || publish
                        .properties
                        .as_ref()
                        .is_some_and(|properties| properties.topic_alias.is_some())
                {
                    return Err(CentralError::Protocol);
                }
                match publish.qos {
                    QoS::AtMostOnce => self.message(publish, on_event)?,
                    QoS::AtLeastOnce => {
                        if publish.pkid == 0 {
                            return Err(CentralError::Protocol);
                        }
                        self.enqueue(
                            Packet::PubAck(PubAck::new(publish.pkid)),
                            Instant::now() + self.timing.request,
                            None,
                            None,
                        )?;
                        self.message(publish, on_event)?;
                    }
                    QoS::ExactlyOnce => {
                        if publish.pkid == 0 {
                            return Err(CentralError::Protocol);
                        }
                        if let Some(previous) = self.incoming.get(&publish.pkid) {
                            if !publish.dup
                                || previous.topic != publish.topic
                                || previous.payload != publish.payload
                            {
                                return Err(CentralError::Protocol);
                            }
                        } else {
                            if self.incoming.len() >= LIMIT {
                                return Err(CentralError::Protocol);
                            }
                            self.incoming.insert(publish.pkid, publish.clone());
                        }
                        self.enqueue(
                            Packet::PubRec(PubRec::new(publish.pkid)),
                            Instant::now() + self.timing.request,
                            None,
                            None,
                        )?;
                    }
                }
            }
            Packet::PubRel(ack) => {
                if ack.pkid == 0 || ack.reason != PubRelReason::Success {
                    return Err(CentralError::Protocol);
                }
                if let Some(publish) = self.incoming.remove(&ack.pkid) {
                    self.message(publish, on_event)?;
                }
                self.enqueue(
                    Packet::PubComp(PubComp::new(ack.pkid)),
                    Instant::now() + self.timing.request,
                    None,
                    None,
                )?;
            }
            Packet::PingResp => {
                self.ping_deadline = None;
            }
            Packet::Disconnect(packet) => {
                return Err(
                    if packet.reason_code == DisconnectReasonCode::NormalDisconnection {
                        CentralError::Disconnected
                    } else {
                        CentralError::Rejected(packet.reason_code as i64)
                    },
                );
            }
            _ => return Err(CentralError::Protocol),
        }
        Ok(())
    }
    fn message(
        &mut self,
        publish: Publish,
        on_event: &mut impl FnMut(CentralEvent),
    ) -> Result<(), CentralError> {
        let topic = publish.topic.as_str();
        let reply = topic == self.reply_topic;
        let list = topic == self.list_topic;
        let property = self.property_topics.get(topic).cloned();
        if !reply && !list && property.is_none() {
            return Ok(());
        }
        let message = wire::decode_mips(&publish.payload)?;
        if reply {
            if let Some(request) = self.pending.remove(&message.id) {
                let result = if Instant::now() >= request.deadline {
                    Err(CentralError::Timeout)
                } else {
                    parse_response(&request.kind, &message.payload)
                };
                log::debug!(
                    "Gateway reply: operation={}, did={}, request_id={}, error={:?}",
                    request.kind.operation(),
                    request.kind.did(),
                    message.id,
                    result.as_ref().err()
                );
                self.finish(request, result);
            }
        } else if list {
            let dids = message
                .payload
                .get("devList")
                .and_then(Value::as_array)
                .ok_or(CentralError::Protocol)?
                .iter()
                .map(|did| {
                    did.as_str()
                        .filter(|did| valid_identifier(did))
                        .map(str::to_owned)
                        .ok_or(CentralError::Protocol)
                })
                .collect::<Result<Vec<_>, _>>()?;
            on_event(CentralEvent::DeviceListChanged { dids });
        } else if let Some((did, binding)) = property {
            validate_identity(&message.payload, &did, binding, true)?;
            let value = message
                .payload
                .get("value")
                .and_then(Value::as_bool)
                .ok_or(CentralError::Protocol)?;
            on_event(CentralEvent::Property {
                did,
                binding,
                value,
            });
        }
        Ok(())
    }
}
