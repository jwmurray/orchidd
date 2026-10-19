// Shelly plug script: forward the orchid-room BLU H&T Display's Bluetooth
// broadcasts to orchidd, which decodes, stores and graphs them.
//
// Install on the heater plug (Bluetooth enabled), named "blu-forward", set to
// run on boot. ORCHIDD is the iMac's LAN address, the same host the Wi-Fi
// sensor's outbound websocket points at.

let SENSOR = "c0:2c:ed:49:9c:b8";
let ORCHIDD = "http://192.168.0.15:8000/api/blu";

let lastSent = "";

function hex(s) {
  let out = "";
  for (let i = 0; i < s.length; i++) {
    let b = s.at(i);
    out += (b < 16 ? "0" : "") + b.toString(16);
  }
  return out;
}

BLE.Scanner.Subscribe(function (ev, res) {
  if (ev !== BLE.Scanner.SCAN_RESULT || !res || res.addr !== SENSOR) return;
  if (!res.service_data || !res.service_data.fcd2) return;
  let data = hex(res.service_data.fcd2);
  // The sensor repeats each broadcast; send each distinct one once.
  if (data === lastSent) return;
  lastSent = data;
  Shelly.call("HTTP.POST", {
    url: ORCHIDD,
    body: JSON.stringify({ data: data }),
    content_type: "application/json",
  });
});

if (!BLE.Scanner.isRunning()) {
  BLE.Scanner.Start({ duration_ms: BLE.Scanner.INFINITE_SCAN, active: false });
}
