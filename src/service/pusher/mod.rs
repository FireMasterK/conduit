mod data;
pub use data::Data;
use ruma::events::AnySyncTimelineEvent;

use crate::{services, Error, PduEvent, Result};
use bytes::BytesMut;
use ruma::{
    api::{
        client::push::{set_pusher, Pusher, PusherKind},
        push_gateway::send_event_notification::{
            self,
            v1::{Device, Notification, NotificationCounts, NotificationPriority},
        },
        IncomingResponse, MatrixVersion, OutgoingRequest, SendAccessToken,
    },
    events::{
        room::{name::RoomNameEventContent, power_levels::RoomPowerLevelsEventContent},
        StateEventType, TimelineEventType,
    },
    push::{Action, PushConditionRoomCtx, PushFormat, Ruleset, Tweak},
    serde::Raw,
    uint, RoomId, UInt, UserId,
};

use std::{fmt::Debug, mem};
use tracing::{info, warn};

pub struct Service {
    pub db: &'static dyn Data,
}

impl Service {
    pub fn set_pusher(&self, sender: &UserId, pusher: set_pusher::v3::PusherAction) -> Result<()> {
        self.db.set_pusher(sender, pusher)
    }

    pub fn get_pusher(&self, sender: &UserId, pushkey: &str) -> Result<Option<Pusher>> {
        self.db.get_pusher(sender, pushkey)
    }

    pub fn get_pushers(&self, sender: &UserId) -> Result<Vec<Pusher>> {
        self.db.get_pushers(sender)
    }

    pub fn get_pushkeys(&self, sender: &UserId) -> Box<dyn Iterator<Item = Result<String>>> {
        self.db.get_pushkeys(sender)
    }

    #[tracing::instrument(skip(self, destination, request))]
    pub async fn send_request<T: OutgoingRequest>(
        &self,
        destination: &str,
        request: T,
    ) -> Result<T::IncomingResponse>
    where
        T: Debug,
    {
        let destination = destination.replace("/_matrix/push/v1/notify", "");

        let http_request = request
            .try_into_http_request::<BytesMut>(
                &destination,
                SendAccessToken::IfRequired(""),
                &[MatrixVersion::V1_0],
            )
            .map_err(|e| {
                warn!("Failed to find destination {}: {}", destination, e);
                Error::BadServerResponse("Invalid destination")
            })?
            .map(|body| body.freeze());

        let reqwest_request = reqwest::Request::try_from(http_request)
            .expect("all http requests are valid reqwest requests");

        // TODO: we could keep this very short and let expo backoff do it's thing...
        //*reqwest_request.timeout_mut() = Some(Duration::from_secs(5));

        let url = reqwest_request.url().clone();
        let response = services()
            .globals
            .default_client()
            .execute(reqwest_request)
            .await;

        match response {
            Ok(mut response) => {
                // reqwest::Response -> http::Response conversion
                let status = response.status();
                let mut http_response_builder = http::Response::builder()
                    .status(status)
                    .version(response.version());
                mem::swap(
                    response.headers_mut(),
                    http_response_builder
                        .headers_mut()
                        .expect("http::response::Builder is usable"),
                );

                let body = response.bytes().await.unwrap_or_else(|e| {
                    warn!("server error {}", e);
                    Vec::new().into()
                }); // TODO: handle timeout

                if status != 200 {
                    info!(
                        "Push gateway returned bad response {} {}\n{}\n{:?}",
                        destination,
                        status,
                        url,
                        crate::utils::string_from_bytes(&body)
                    );
                }

                let response = T::IncomingResponse::try_from_http_response(
                    http_response_builder
                        .body(body)
                        .expect("reqwest body is valid http body"),
                );
                response.map_err(|_| {
                    info!(
                        "Push gateway returned invalid response bytes {}\n{}",
                        destination, url
                    );
                    Error::BadServerResponse("Push gateway returned bad response.")
                })
            }
            Err(e) => {
                warn!("Could not send request to pusher {}: {}", destination, e);
                Err(e.into())
            }
        }
    }

    #[tracing::instrument(skip(self, user, unread, pusher, ruleset, pdu))]
    pub async fn send_push_notice(
        &self,
        user: &UserId,
        unread: UInt,
        pusher: &Pusher,
        ruleset: Ruleset,
        pdu: &PduEvent,
    ) -> Result<()> {
        let mut notify = None;
        let mut tweaks = Vec::new();

        let power_levels: RoomPowerLevelsEventContent = services()
            .rooms
            .state_accessor
            .room_state_get(&pdu.room_id, &StateEventType::RoomPowerLevels, "")?
            .map(|ev| {
                serde_json::from_str(ev.content.get())
                    .map_err(|_| Error::bad_database("invalid m.room.power_levels event"))
            })
            .transpose()?
            .unwrap_or_default();

        for action in self.get_actions(
            user,
            &ruleset,
            &power_levels,
            &pdu.to_sync_room_event(),
            &pdu.room_id,
        )? {
            let n = match action {
                Action::Notify => true,
                Action::SetTweak(tweak) => {
                    tweaks.push(tweak.clone());
                    continue;
                }
                _ => false,
            };

            if notify.is_some() {
                return Err(Error::bad_database(
                    r#"Malformed pushrule contains more than one of these actions: ["dont_notify", "notify", "coalesce"]"#,
                ));
            }

            notify = Some(n);
        }

        if notify == Some(true) {
            self.send_notice(unread, pusher, tweaks, pdu).await?;
        }
        // Else the event triggered no actions

        Ok(())
    }

    #[tracing::instrument(skip(self, user, ruleset, pdu))]
    pub fn get_actions<'a>(
        &self,
        user: &UserId,
        ruleset: &'a Ruleset,
        power_levels: &RoomPowerLevelsEventContent,
        pdu: &Raw<AnySyncTimelineEvent>,
        room_id: &RoomId,
    ) -> Result<&'a [Action]> {
        let ctx = PushConditionRoomCtx {
            room_id: room_id.to_owned(),
            member_count: 10_u32.into(), // TODO: get member count efficiently
            user_id: user.to_owned(),
            user_display_name: services()
                .users
                .displayname(user)?
                .unwrap_or_else(|| user.localpart().to_owned()),
            users_power_levels: power_levels.users.clone(),
            default_power_level: power_levels.users_default,
            notification_power_levels: power_levels.notifications.clone(),
        };

        Ok(ruleset.get_actions(pdu, &ctx))
    }

    #[tracing::instrument(skip(self, unread, pusher, tweaks, event))]
    async fn send_notice(
        &self,
        unread: UInt,
        pusher: &Pusher,
        tweaks: Vec<Tweak>,
        event: &PduEvent,
    ) -> Result<()> {
        // TODO: email
        match &pusher.kind {
            PusherKind::Http(http) => {
                // TODO:
                // Two problems with this
                // 1. if "event_id_only" is the only format kind it seems we should never add more info
                // 2. can pusher/devices have conflicting formats
                let event_id_only = http.format == Some(PushFormat::EventIdOnly);

                let mut device = Device::new(pusher.ids.app_id.clone(), pusher.ids.pushkey.clone());
                device.data.default_payload = http.default_payload.clone();
                device.data.format = http.format.clone();

                // Tweaks are only added if the format is NOT event_id_only
                if !event_id_only {
                    device.tweaks = tweaks.clone();
                }

                let d = vec![device];
                let mut notifi = Notification::new(d);

                notifi.prio = NotificationPriority::Low;
                notifi.event_id = Some((*event.event_id).to_owned());
                notifi.room_id = Some((*event.room_id).to_owned());
                // TODO: missed calls
                notifi.counts = NotificationCounts::new(unread, uint!(0));

                if event.kind == TimelineEventType::RoomEncrypted
                    || tweaks
                        .iter()
                        .any(|t| matches!(t, Tweak::Highlight(true) | Tweak::Sound(_)))
                {
                    notifi.prio = NotificationPriority::High
                }

                if event_id_only {
                    self.send_request(&http.url, send_event_notification::v1::Request::new(notifi))
                        .await?;
                } else {
                    notifi.sender = Some(event.sender.clone());
                    notifi.event_type = Some(event.kind.clone());
                    notifi.content = serde_json::value::to_raw_value(&event.content).ok();

                    if event.kind == TimelineEventType::RoomMember {
                        notifi.user_is_target =
                            event.state_key.as_deref() == Some(event.sender.as_str());
                    }

                    notifi.sender_display_name = services().users.displayname(&event.sender)?;

                    let room_name = if let Some(room_name_pdu) = services()
                        .rooms
                        .state_accessor
                        .room_state_get(&event.room_id, &StateEventType::RoomName, "")?
                    {
                        serde_json::from_str::<RoomNameEventContent>(room_name_pdu.content.get())
                            .map_err(|_| {
                                Error::bad_database("Invalid room name event in database.")
                            })?
                            .name
                    } else {
                        None
                    };

                    notifi.room_name = room_name;

                    self.send_request(&http.url, send_event_notification::v1::Request::new(notifi))
                        .await?;
                }

                Ok(())
            }
            // TODO: Handle email
            PusherKind::Email(_) => Ok(()),
            _ => Ok(()),
        }
    }
}
