//! User-facing texts on the PC (tray, toasts) in German and English. The iPad page has its own
//! table in web/index.html. Logs and CLI help stay English.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    De,
    En,
}

impl Lang {
    /// Windows display language; German for any German locale, English otherwise.
    #[cfg(windows)]
    pub fn detect() -> Self {
        // LANG_GERMAN = 0x07 is the primary language id in the low 10 bits of the LANGID.
        let langid = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
        Self::from_langid(langid)
    }

    pub fn from_langid(langid: u16) -> Self {
        if langid & 0x3ff == 0x07 {
            Lang::De
        } else {
            Lang::En
        }
    }
}

/// Kind of sending device, as the page reports it in "hello". Only these fixed values reach
/// the tray and the notifications, never text from the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Device {
    IPhone,
    IPad,
    Android,
    #[default]
    Other,
}

impl Device {
    pub fn from_hello(value: Option<&str>) -> Self {
        match value {
            Some("iphone") => Device::IPhone,
            Some("ipad") => Device::IPad,
            Some("android") => Device::Android,
            _ => Device::Other,
        }
    }
}

/// All PC-side strings. `&'static str` where fixed, functions where values are inserted.
pub struct Texts {
    pub lang: Lang,
}

impl Texts {
    pub fn new(lang: Lang) -> Self {
        Self { lang }
    }

    fn pick(&self, de: &'static str, en: &'static str) -> &'static str {
        match self.lang {
            Lang::De => de,
            Lang::En => en,
        }
    }

    pub fn waiting(&self) -> &'static str {
        self.pick(
            "CouchMic: wartet auf Verbindung",
            "CouchMic: waiting for a connection",
        )
    }

    /// Device name at the start of a sentence.
    fn device(&self, d: Device) -> &'static str {
        match d {
            Device::IPhone => "iPhone",
            Device::IPad => "iPad",
            Device::Android => self.pick("Android-Gerät", "Android device"),
            Device::Other => self.pick("Gerät", "Device"),
        }
    }

    /// "iPhone verbunden", or "2 Geräte verbunden" with several senders.
    fn connected(&self, d: Device, clients: u32) -> String {
        match (self.lang, clients) {
            (Lang::De, 0 | 1) => format!("{} verbunden", self.device(d)),
            (Lang::En, 0 | 1) => format!("{} connected", self.device(d)),
            (Lang::De, n) => format!("{n} Geräte verbunden"),
            (Lang::En, n) => format!("{n} devices connected"),
        }
    }

    pub fn menu_auto_switch(&self) -> &'static str {
        self.pick(
            "Standard-Mikrofon automatisch umschalten",
            "Switch default microphone automatically",
        )
    }

    pub fn menu_open_ui(&self) -> &'static str {
        self.pick("Web-Oberfläche öffnen", "Open web page")
    }

    pub fn menu_open_logs(&self) -> &'static str {
        self.pick("Log-Ordner öffnen", "Open log folder")
    }

    pub fn menu_quit(&self) -> &'static str {
        self.pick("Beenden", "Quit")
    }

    pub fn status_connected(&self, device: Device, path: &str, clients: u32) -> String {
        format!("CouchMic: {} ({path})", self.connected(device, clients))
    }

    pub fn tooltip_idle(&self, device: &str, switching: bool) -> String {
        match self.lang {
            Lang::De => format!(
                "{}\nAusgabe: {device}\nMikrofon-Umschaltung: {}",
                self.waiting(),
                if switching { "an" } else { "aus" }
            ),
            Lang::En => format!(
                "{}\nOutput: {device}\nMicrophone switching: {}",
                self.waiting(),
                if switching { "on" } else { "off" }
            ),
        }
    }

    pub fn tooltip_connected(
        &self,
        head: &str,
        buffered_ms: f64,
        target_ms: f64,
        loss_pct: f64,
        underruns: u64,
    ) -> String {
        match self.lang {
            Lang::De => format!(
                "{head}\nPuffer {buffered_ms:.0} ms (Ziel {target_ms:.0}), Verlust {loss_pct:.1} %, Underruns {underruns}"
            ),
            Lang::En => format!(
                "{head}\nBuffer {buffered_ms:.0} ms (target {target_ms:.0}), loss {loss_pct:.1} %, underruns {underruns}"
            ),
        }
    }

    pub fn toast_connected(&self, device: Device, mic: &str, switched: bool) -> String {
        let head = self.connected(device, 1);
        match (self.lang, switched) {
            (Lang::De, true) => format!("{head}. {mic} ist jetzt das Standard-Mikrofon."),
            (Lang::De, false) => format!("{head}. Mikrofon: {mic}."),
            (Lang::En, true) => format!("{head}. {mic} is now the default microphone."),
            (Lang::En, false) => format!("{head}. Microphone: {mic}."),
        }
    }

    pub fn toast_disconnected(&self, device: Device, restored: bool) -> String {
        let d = self.device(device);
        match (self.lang, restored) {
            (Lang::De, true) => format!("{d} getrennt. Das vorige Mikrofon ist wieder Standard."),
            (Lang::De, false) => format!("{d} getrennt."),
            (Lang::En, true) => {
                format!("{d} disconnected. The previous microphone is the default again.")
            }
            (Lang::En, false) => format!("{d} disconnected."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn german_locales_map_to_german() {
        // de-DE 0x0407, de-AT 0x0C07, de-CH 0x0807
        for id in [0x0407u16, 0x0C07, 0x0807] {
            assert_eq!(Lang::from_langid(id), Lang::De, "{id:#06x}");
        }
        // en-US 0x0409, fr-FR 0x040C
        for id in [0x0409u16, 0x040C] {
            assert_eq!(Lang::from_langid(id), Lang::En, "{id:#06x}");
        }
    }

    #[test]
    fn texts_fill_in_values() {
        let de = Texts::new(Lang::De);
        let en = Texts::new(Lang::En);
        assert_eq!(
            de.toast_connected(Device::IPhone, "CABLE Output", true),
            "iPhone verbunden. CABLE Output ist jetzt das Standard-Mikrofon."
        );
        assert_eq!(
            en.toast_connected(Device::IPad, "CABLE Output", true),
            "iPad connected. CABLE Output is now the default microphone."
        );
        assert_eq!(
            de.toast_disconnected(Device::Android, false),
            "Android-Gerät getrennt."
        );
        assert_eq!(
            de.status_connected(Device::IPad, "PCM", 1),
            "CouchMic: iPad verbunden (PCM)"
        );
        assert_eq!(
            en.status_connected(Device::IPhone, "PCM", 2),
            "CouchMic: 2 devices connected (PCM)"
        );
    }

    #[test]
    fn only_known_device_values_are_used() {
        assert_eq!(Device::from_hello(Some("iphone")), Device::IPhone);
        assert_eq!(Device::from_hello(Some("ipad")), Device::IPad);
        assert_eq!(Device::from_hello(Some("android")), Device::Android);
        for v in [
            None,
            Some(""),
            Some("iPhone"),
            Some("evil\nline"),
            Some("other"),
        ] {
            assert_eq!(Device::from_hello(v), Device::Other, "{v:?}");
        }
        let en = Texts::new(Lang::En);
        assert!(en
            .toast_connected(Device::Other, "CABLE Output", false)
            .starts_with("Device connected."));
    }
}
