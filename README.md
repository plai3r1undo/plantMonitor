# plantMonitor

House-plant monitoring: one ESP32 per pot measures light, air temperature/humidity and soil moisture, and sends readings over WiFi to a central Rust server. The server compares them with each species' optimal ranges, sends push notifications when a plant needs attention, serves a password-protected web dashboard (where you also manage plants and species), and exposes a JSON/CSV API for scripts.

```
 ┌──────────── pot ────────────┐          ┌──────────────── DMZ host ────────────────┐
 │ BH1750  DHT22  HW-390       │HTTPS, LAN│ Caddy (TLS) ──► plant-server (Rust)      │
 │    └──── ESP32 ────┘        │ ───────► │                  SQLite · alerts · UI    │──► ntfy ──► phone
 │  wake → measure → POST →    │          └──────────────────────────────────────────┘
 │  deep sleep (10 min)        │                     ▲ browser (login) / curl (token)
 └─────────────────────────────┘
```

```
firmware/   ESP32 sensor node (C++, Arduino framework, PlatformIO)
server/     Rust server + web UI (plain HTML/CSS/JS, embedded in the binary)
deploy/     systemd unit, Caddy reverse-proxy config, install script
build.sh    builds/flashes the firmware and builds/runs the server
```

## Wiring (generic ESP32 DevKit, WROOM-32)

| Sensor          | Sensor pin | ESP32 pin |
|-----------------|------------|-----------|
| BH1750          | VCC / GND  | 3V3 / GND |
|                 | SDA        | GPIO 21   |
|                 | SCL        | GPIO 22   |
|                 | ADDR       | GND / unconnected (address 0x23) |
| DHT22           | VCC / GND  | 3V3 / GND |
|                 | DATA       | GPIO 4 (10 kΩ pull-up to 3V3 if the module has none) |
| HW-390 (soil)   | VCC / GND  | 3V3 / GND |
|                 | AOUT       | GPIO 34   |

Power the HW-390 from **3V3**, not 5 V, so its output stays inside the ESP32 ADC range. The soil sensor must be on an ADC1 pin (GPIO 32–39) because ADC2 can't be used while WiFi is on. Pins are set in `firmware/include/config.h`.

## The firmware (no C++ knowledge needed)

You normally only edit two files, both made of `#define NAME value` lines:

- `firmware/include/secrets.h`: WiFi name/password, server URL, device key
- `firmware/include/config.h`: pins and measurement interval (defaults match the wiring above)

`src/main.cpp` (~170 lines) does: read sensors → connect WiFi → POST JSON → deep sleep. Every node flashed with the same firmware reports as `plant-<last 3 MAC bytes>`.

## Quick start (testing on your LAN)

```sh
cp server/config.example.toml server/config.toml
#   edit: device_key (openssl rand -hex 24), listen = "0.0.0.0:8080", secure_cookies = false
./build.sh user-add admin        # create your login
./build.sh run-server            # http://<this-machine>:8080

./build.sh setup                 # installs PlatformIO into .tools/ (first time only)
./build.sh build                 # first run creates firmware/include/secrets.h: fill it in
./build.sh deploy -p /dev/ttyUSB0   # flash + serial monitor
```

To flash on Fedora you need to be in the `dialout` group: `sudo usermod -aG dialout $USER`, then log out and back in.

New sensors show up on the dashboard under **Unassigned devices**. Click **Add as plant**, pick the species, and calibrate the soil sensor. The plant form has **Use latest** buttons: hold the probe in dry air, wait for a reading, press *Use latest* next to *Dry*, then do the same in a glass of water for *Wet*.

Later changes, all from the **Plants** tab:

- **Recalibrate** a plant's soil sensor: a guided dialog takes the new dry and wet values. Until you save or cancel (at most 30 min), the probe's readings stay out of the plant's history and alerts.
- **Move a sensor** to another plant: edit the plant that should get it and pick the device. Readings belong to the plant they were taken in, so the old plant keeps its history (it shows *No sensor* until it gets one), and the sensor's calibration is copied along.
- **Delete an offline device** (broken or retired): it is removed from its plant, which keeps its history. If it reports again it shows up as a new device.

**Soil drying check.** From the soil moisture history, the server finds waterings (sharp rises) and measures how long the soil takes to dry down to the species' minimum. Each species has an expected drying time (*Dries in* on the Species tab). Soil that stays wet much longer (e.g. a cactus in a dense mix, or a pot without drainage) or dries much faster (mix too coarse, pot too small) gets a hint on the plant's card, with details in its chart view. It needs at least one thorough watering followed by a few days of readings, and isn't judged while a plant is dormant.

## Production: exposing it in a DMZ

### 1. Install the server

On the DMZ machine (Fedora example):

```sh
./build.sh server
sudo deploy/install.sh            # user, dirs, config with random device key, systemd unit
sudo -u plantmon plant-server -c /etc/plant-monitor/config.toml user-add <name>
sudo dnf install caddy
sudo cp deploy/Caddyfile /etc/caddy/Caddyfile   # edit: LAN name, optional public site
sudo systemctl enable --now caddy
sudo firewall-cmd --permanent --add-service=https && sudo firewall-cmd --reload
```

The server only listens on `127.0.0.1:8080`. Caddy handles HTTPS and is the only thing reachable from the network.

`deploy/Caddyfile` has two sites:

- **LAN site (required)**, e.g. `plants.home.arpa`: used by the sensors and by browsers at home. Caddy is its own certificate authority (`tls internal`), so it needs no domain and no internet. Add a DNS entry for the name on your router, or use the machine's fixed IP as the site address.
- **Public site (optional)**, e.g. `plants.example.com`: only if you want the dashboard from outside. Caddy gets a free Let's Encrypt certificate. It doesn't serve the sensor endpoint.

### 2. HTTPS for the sensors

The sensors only talk to the LAN site. They use HTTPS and verify the certificate against Caddy's root certificate, which is built into the firmware, so the device key can't be sniffed and nobody can impersonate the server:

```sh
sudo cat /var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt > firmware/root_ca.pem
```

Then set `SERVER_URL "https://plants.home.arpa"` (or `https://<fixed IP>`) and `DEVICE_KEY` (from `/etc/plant-monitor/config.toml`) in `secrets.h` and reflash. The root certificate is valid for 10 years; Caddy renews the server certificates under it by itself.

The server also refuses readings from any address outside your LAN (`device_networks` in config.toml, default: the private ranges 10/8, 172.16/12, 192.168/16 and IPv6 ULA), so even with the public site enabled, nobody on the internet can submit readings.

### 3. Firewall rules on your router

| From → To | Allow |
|---|---|
| Internet → DMZ host | nothing; only if you enable the public site: TCP 443 + 80 (80 for Let's Encrypt) |
| LAN → DMZ host | TCP 443 (sensors, browser), TCP 22 from your PC only |
| DMZ host → LAN | **nothing** (if the host is compromised, it can't reach your network) |
| DMZ host → Internet | TCP 443 (ntfy, OS updates, Let's Encrypt), DNS, NTP |

### 4. Keep the host healthy

Run the read-only check script now and then (it changes nothing):

```sh
sudo deploy/check-host.sh --lan 192.168.1.1   # IP of your router or a LAN PC
```

It checks updates, SELinux, firewall, open ports, SSH settings, accounts, the plant-server and Caddy setup, failed logins, and (with `--lan`) that the DMZ host can't connect into your LAN.

- SSH with keys only (`PasswordAuthentication no`), no root login
- Automatic security updates: `sudo dnf install dnf-automatic && sudo systemctl enable --now dnf-automatic-install.timer`
- Nothing else running on the DMZ host (disable mDNS/LLMNR: `systemctl disable --now avahi-daemon`, `LLMNR=no` and `MulticastDNS=no` in `/etc/systemd/resolved.conf`)
- Back up `/var/lib/plant-monitor/plants.db` and `/etc/plant-monitor/config.toml`
- Watch failed logins: `journalctl -u plant-server | grep "failed login"`

## Security model

What the server does:

| Threat | Protection |
|---|---|
| Password guessing | Argon2id password hashes; 5 failed logins per IP (per /64 for IPv6) and 30 overall per 15 min, then HTTP 429; unknown usernames take as long as wrong passwords |
| Stolen session / DB leak | Random 256-bit session and API tokens, stored only as SHA-256 hashes; cookie is `HttpOnly`, `Secure`, `SameSite=Strict`, `__Host-` prefixed; sessions expire (14 days); changing your password logs out every other session |
| Cross-site request forgery | `SameSite=Strict` plus a required `X-Requested-With` header on every state-changing request |
| Script injection (XSS) | Every value is HTML-escaped; strict Content-Security-Policy (no inline scripts or styles, no third-party content) |
| Clickjacking, sniffing, leaks | `frame-ancestors 'none'`, `X-Frame-Options`, `nosniff`, `no-referrer`, HSTS (via Caddy) |
| Fake sensor data | Readings accepted only from LAN addresses, with the device key (compared in constant time), sent only over verified HTTPS |
| Oversized / slow requests | 16 KB body limit, 30 s request timeout, Caddy in front |
| Server compromise | Rust (memory safe), no unsafe code, runs as an unprivileged user under systemd sandboxing with no capabilities and a read-only filesystem except its data directory; DMZ firewall blocks access to your LAN |
| SQL injection | All queries are parameterized |

What's left, honestly:

- **All sensors share one device key.** Someone with access to your LAN who takes a sensor out of a pot and reads its flash can submit fake readings, but they can't log in or read any data. You can rotate the key in config.toml and reflash.
- **The global login limit can be used to lock you out** by an attacker who keeps failing logins. Sessions last 14 days, so this only blocks *new* logins, and the attacker still can't get in.
- **No two-factor authentication.** Use a long, unique password (a password manager).
- **Dependencies** (axum, rusqlite, etc.) can have vulnerabilities. Rebuild and redeploy now and then (`cargo update && ./build.sh server && sudo deploy/install.sh`); `cargo install cargo-audit && cargo audit` checks for known issues.
- If only you need remote access, a **VPN (WireGuard)** into your network is still safer than any exposed web app. Everything here works the same over a VPN.

## Notifications (ntfy)

[ntfy.sh](https://ntfy.sh) is free and needs no account. Install the ntfy app on your phone and subscribe to a long random topic name, then add it to the config:

```toml
public_url = "https://plants.example.com"   # where tapping the notification goes

[ntfy]
server = "https://ntfy.sh"
topic = "plants-3f9c0e1a7b2d4c86e5a1"         # "plants-" + openssl rand -hex 10
```

On the public server, anyone who knows the topic name can read it, so treat the name like a password. You can also self-host ntfy and set `token`. A problem is notified after it has lasted `alert_after_minutes` (default 30), repeated every `remind_after_hours` (default 24), and you get a ✅ when it's fixed. Alerts can be turned off per plant. **Settings → Send test notification** checks the setup.

## How plants are evaluated

Temperature, humidity and soil moisture use the latest reading. Light uses the **average lux over the last 24 h, counting only readings ≥ 50 lx**, so night readings don't flag every plant as too dark. A plant's status is `ok`, `attention` (something out of range; the UI shows advice), `offline` (no reading for `offline_after_minutes`), `no_data`, or `no_sensor` (its sensor was moved or deleted).

## API

Create a read-only token under **Settings → API tokens**, then:

```sh
T=your-token; S=https://plants.example.com
curl -s -H "Authorization: Bearer $T" $S/api/plants | jq -r '.[] | "\(.name): \(.status)"'
curl -s -H "Authorization: Bearer $T" "$S/api/plants/1/readings?hours=168&format=csv" > week.csv
```

| Method | Path | Description |
|---|---|---|
| GET  | `/api/plants` | All plants with latest reading, checks and status |
| GET  | `/api/plants/{id}` | One plant |
| GET  | `/api/plants/{id}/readings?hours=24&format=json\|csv` | History (max 1 year) |
| GET  | `/api/species` | Species and their optimal ranges |
| GET  | `/api/devices` | Sensors that are assigned or have unassigned readings, with `online` |
| GET  | `/api/health` | Liveness check (no auth) |

Tokens are read-only. Creating, editing and deleting (plants, species, tokens) needs a logged-in browser session.

## Server CLI

```sh
plant-server -c config.toml user-add NAME      # create a login (prompts for password, min 12 chars; shorter only after confirming, and the UI then warns until it is changed)
plant-server -c config.toml user-passwd NAME   # reset a password, ends all its sessions
plant-server -c config.toml user-del NAME
plant-server -c config.toml user-list
```
