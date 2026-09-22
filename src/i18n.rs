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
        self.pick("Glass Mic: wartet auf iPad", "Glass Mic: waiting for iPad")
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

    pub fn status_connected(&self, path: &str, clients: u32) -> String {
        match self.lang {
            Lang::De => format!(
                "Glass Mic: iPad verbunden ({path}, {clients} Client{})",
                if clients == 1 { "" } else { "s" }
            ),
            Lang::En => format!(
                "Glass Mic: iPad connected ({path}, {clients} client{})",
                if clients == 1 { "" } else { "s" }
            ),
        }
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
        path: &str,
        buffered_ms: f64,
        target_ms: f64,
        loss_pct: f64,
        underruns: u64,
    ) -> String {
        match self.lang {
            Lang::De => format!(
                "Glass Mic: iPad verbunden ({path})\nPuffer {buffered_ms:.0} ms (Ziel {target_ms:.0}), Verlust {loss_pct:.1} %, Underruns {underruns}"
            ),
            Lang::En => format!(
                "Glass Mic: iPad connected ({path})\nBuffer {buffered_ms:.0} ms (target {target_ms:.0}), loss {loss_pct:.1} %, underruns {underruns}"
            ),
        }
    }

    pub fn toast_connected(&self, mic: &str, switched: bool) -> String {
        match (self.lang, switched) {
            (Lang::De, true) => format!("iPad verbunden. {mic} ist jetzt das Standard-Mikrofon."),
            (Lang::De, false) => format!("iPad verbunden. Mikrofon: {mic}."),
            (Lang::En, true) => format!("iPad connected. {mic} is now the default microphone."),
            (Lang::En, false) => format!("iPad connected. Microphone: {mic}."),
        }
    }

    pub fn toast_disconnected(&self, restored: bool) -> &'static str {
        match restored {
            true => self.pick(
                "iPad getrennt. Das vorige Mikrofon ist wieder Standard.",
                "iPad disconnected. The previous microphone is the default again.",
            ),
            false => self.pick("iPad getrennt.", "iPad disconnected."),
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
        assert!(de
            .toast_connected("CABLE Output", true)
            .contains("Standard-Mikrofon"));
        assert!(en
            .toast_connected("CABLE Output", true)
            .contains("default microphone"));
        assert!(en.status_connected("PCM", 2).ends_with("2 clients)"));
        assert!(de.status_connected("PCM", 1).ends_with("1 Client)"));
    }
}
