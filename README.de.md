# CouchMic

Das iPad oder iPhone als Mikrofon für den Windows-PC. Auf dem iPad wird keine App installiert:
Safari schickt das Mikrofon per WebRTC durch dein [Tailscale](https://tailscale.com)-Netz,
CouchMic spielt es in [VB-CABLE](https://vb-audio.com/Cable/), und jede Windows-App kann
„CABLE Output“ als Mikrofon benutzen.

Gedacht für Remote-Desktop-Setups (Moonlight/Sunshine, Jump Desktop, Parsec), bei denen das iPad
Bildschirm und Tastatur ist, der PC aber kein eigenes Mikrofon hat oder in einem anderen Raum steht.

Die ausführliche Dokumentation steht in der [englischen README](README.md). Hier das Wichtigste.

## Voraussetzungen

- Windows 11 x64 (getestet). Windows 10 x64 sollte funktionieren, ist aber ungetestet.
- [VB-CABLE](https://vb-audio.com/Cable/) (kostenlos, Donationware), danach neu starten.
- [Tailscale](https://tailscale.com/download) auf PC und iPad/iPhone im selben Tailnet. In der
  [Admin-Konsole](https://login.tailscale.com/admin/dns) **MagicDNS** und **HTTPS-Zertifikate**
  einschalten. Safari gibt das Mikrofon nur auf HTTPS-Seiten frei, das Zertifikat kommt von
  `tailscale serve`.

## Installation

1. `couchmic-<version>-windows-x64.zip` aus den
   [Releases](https://github.com/nexos-1/couchmic/releases) laden und entpacken.
2. PowerShell in dem Ordner öffnen:
   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install.ps1
   ```
   Das Skript prüft VB-CABLE, kopiert CouchMic nach `%LOCALAPPDATA%\CouchMic`, legt die
   Firewall-Regel für UDP 8322 an (eine Admin-Abfrage), richtet eine geplante Aufgabe ein (Start
   bei Anmeldung, Neustart bei Bedarf) und startet `tailscale serve`. Am Ende steht die Adresse
   für das iPad da, z.B. `https://mein-pc.tail1234.ts.net/`.
3. Auf dem iPad die Adresse in Safari öffnen, **Mikrofon starten**, Zugriff erlauben.
4. Optional: Teilen, **Zum Home-Bildschirm**. Das Symbol öffnet die Seite absichtlich in Safari:
   iOS friert eigenständige Web-Apps im Hintergrund ein, ein Safari-Tab nimmt weiter auf.

Aktualisieren: neues Zip laden, `install.ps1` erneut ausführen.
Entfernen: `powershell -ExecutionPolicy Bypass -File .\install.ps1 -Uninstall` (stellt das vorige
Standard-Mikrofon wieder her und entfernt Aufgabe, Firewall-Regel und `tailscale serve`).

Das Programm ist noch nicht code-signiert; Windows SmartScreen warnt beim ersten Start eventuell
(„Weitere Informationen“, „Trotzdem ausführen“). Die SHA-256-Prüfsumme steht auf der Release-Seite.

## Bedienung

- Mikrofon auf dem iPad starten, sprechen: „CABLE Output“ ist jetzt das Standard-Mikrofon.
- Kurzbefehle-Automation: „Wenn App geöffnet wird“ (z.B. Moonlight), Aktion „URL öffnen“
  `https://<pc>.<tailnet>.ts.net/?autostart=1`; „Wenn App geschlossen wird“ mit `?stop=1`.
- Tray-Menü: Status, automatische Mikrofon-Umschaltung an/aus, Web-Oberfläche, Log-Ordner, Beenden.
- Log: `%LOCALAPPDATA%\CouchMic\couchmic.log`.

## Sicherheit und Datenschutz

- Der Webserver lauscht nur auf `127.0.0.1` und ist nur über `tailscale serve` aus dem eigenen
  Tailnet erreichbar, und dort nur für den eigenen Tailscale-Nutzer des PCs (andere Nutzer eines
  geteilten Tailnets werden abgewiesen).
- **Tailscale Funnel wird immer abgewiesen**, `install.ps1` richtet bei aktivem Funnel nichts ein.
- Fremde Webseiten werden per Host-, Origin- und `Sec-Fetch-Site`-Prüfung abgewiesen; die Seite
  lässt sich nicht einbetten.
- WebRTC-Audio (UDP 8322) lässt die Firewall-Regel nur von Tailscale-Adressen zu.
- Kein Konto, keine Cloud: Das Audio geht vom iPad zum PC und sonst nirgendwohin. Ein paar
  Statusmeldungen der Seite (inklusive User-Agent) landen nur im lokalen Log.

Details in [README.md](README.md) und [SECURITY.md](SECURITY.md).

## Lizenz

MIT, siehe [LICENSE](LICENSE). Hinweise zu enthaltener Fremdsoftware (u.a. libopus) liegen dem
Release als `THIRD-PARTY-LICENSES.html` bei. VB-CABLE ist ein eigenes Produkt von VB-Audio und
nicht enthalten. Tailscale ist eine Marke der Tailscale Inc., iPad, iPhone und Safari sind Marken
der Apple Inc., Windows ist eine Marke der Microsoft Corporation; CouchMic steht mit keinem
dieser Unternehmen in Verbindung.
