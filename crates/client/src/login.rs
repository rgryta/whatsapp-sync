//! The login a resumed device sends inside the handshake: who it is and what it claims to be.
//!
//! It is what WAHA's Baileys sends for this same device today, field for field, so the server
//! sees no change of client when this one takes over the session. `tests/noise.rs` pins the
//! bytes against Baileys' own `generateLoginNode` with WAHA's socket configuration.

use prost::Message;

use crate::proto::{AppVersion, ClientPayload, UserAgent, WebInfo};

const PLATFORM_WEB: i32 = 14;
const RELEASE_CHANNEL_RELEASE: i32 = 0;
const WEB_SUB_PLATFORM_WEB_BROWSER: i32 = 0;
const CONNECT_TYPE_WIFI_UNKNOWN: i32 = 1;
const CONNECT_REASON_USER_ACTIVATED: i32 = 1;

pub struct LoginSettings {
    /// The WhatsApp Web version claimed. WhatsApp retires old ones, so this has to move.
    pub version: [u32; 3],
    pub country: String,
}

impl Default for LoginSettings {
    /// WAHA's: Baileys 7.0.0-rc14's default version, WAHA_NOWEB_WA_VERSION unset.
    fn default() -> Self {
        Self {
            version: [2, 3000, 1_050_293_785],
            country: "US".into(),
        }
    }
}

/// The `ClientPayload` for resuming the paired device `user:device@s.whatsapp.net`.
pub fn client_payload(user: u64, device: u32, settings: &LoginSettings) -> Vec<u8> {
    let [primary, secondary, tertiary] = settings.version;
    ClientPayload {
        username: Some(user),
        passive: Some(true),
        user_agent: Some(UserAgent {
            platform: Some(PLATFORM_WEB),
            app_version: Some(AppVersion {
                primary: Some(primary),
                secondary: Some(secondary),
                tertiary: Some(tertiary),
            }),
            mcc: Some("000".into()),
            mnc: Some("000".into()),
            os_version: Some("0.1".into()),
            device: Some("Desktop".into()),
            os_build_number: Some("0.1".into()),
            release_channel: Some(RELEASE_CHANNEL_RELEASE),
            locale_language_iso6391: Some("en".into()),
            locale_country_iso31661_alpha2: Some(settings.country.clone()),
        }),
        web_info: Some(WebInfo {
            web_sub_platform: Some(WEB_SUB_PLATFORM_WEB_BROWSER),
        }),
        connect_type: Some(CONNECT_TYPE_WIFI_UNKNOWN),
        connect_reason: Some(CONNECT_REASON_USER_ACTIVATED),
        device: Some(device),
        lc: Some(1),
        pull: Some(true),
        lid_db_migrated: Some(true),
    }
    .encode_to_vec()
}
