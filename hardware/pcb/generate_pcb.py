#!/usr/bin/env python3
"""Generates the plant sensor carrier board (KiCad 9) from code.

The ESP32 DevKit V1 (30 pin), the DHT22 module and the BH1750 (GY-302)
module plug into female pin sockets; the HW-390 soil sensor connects to a
3-pin header or to 3 solder holes (use one or the other).

    python3 hardware/pcb/generate_pcb.py   # writes plant-carrier.kicad_pcb

All coordinates are in mm from the top-left corner of the board (y grows
downwards), with the ESP32 antenna pointing to the top edge.
"""
import os
import pcbnew

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "plant-carrier.kicad_pcb")
FP_LIB = "/usr/share/kicad/footprints"

# ---- Board -------------------------------------------------------------------
BOARD_W, BOARD_H = 72.0, 62.0
ORIGIN = (100.0, 60.0)  # where the board sits on the KiCad page

# ---- ESP32 DevKit V1, 30 pins: MEASURE YOUR BOARD ------------------------------
ROW_SPACING = 25.4      # centre-to-centre distance of the two pin rows
ESP_LEFT_X = 9.5        # left pin row
ESP_TOP_Y = 14.0        # pin 1 (EN / D23), antenna end
PITCH = 2.54
ESP_W, ESP_L = 28.3, 51.5          # module outline (silkscreen only)
ESP_PIN1_FROM_TOP = 13.0           # antenna edge to the first pin row

# Pin labels, top (antenna) to bottom (USB), board seen from above.
ESP_LEFT = ["EN", "VP", "VN", "D34", "D35", "D32", "D33", "D25", "D26", "D27",
            "D14", "D12", "D13", "GND", "VIN"]
ESP_RIGHT = ["D23", "D22", "TX0", "RX0", "D21", "D19", "D18", "D5", "D17", "D16",
             "D4", "D2", "D15", "GND", "3V3"]

ESP_RIGHT_X = ESP_LEFT_X + ROW_SPACING

# ---- Sensors ------------------------------------------------------------------
SENSOR_CX = 55.5          # centre line of the sensor column
BH_Y = 9.0                # BH1750 socket row (top right)
BH_WIRE_Y = BH_Y + 3.06   # BH1750 solder holes, parallel to the socket
DHT_Y = 56.0              # DHT22 socket row (bottom right)
SOIL_HDR_X, SOIL_PAD_X = 2.54, 5.6   # soil header / solder holes (left edge)
SOIL_Y = ESP_TOP_Y + 3 * PITCH       # level with D34
SLOT = (39.5, 27.0, 41.0, 43.0)      # thermal slot between ESP32 and DHT22

SIGNAL_W, POWER_W = 0.4, 0.6

board = pcbnew.BOARD()


def mm(v):
    return pcbnew.FromMM(v)


def pt(x, y):
    return pcbnew.VECTOR2I(mm(ORIGIN[0] + x), mm(ORIGIN[1] + y))


nets = {}


def net(name):
    if name not in nets:
        n = pcbnew.NETINFO_ITEM(board, name)
        board.Add(n)
        nets[name] = n
    return nets[name]


def footprint(lib, name, ref, value, x, y, rot=0, pad_nets=None):
    fp = pcbnew.FootprintLoad(os.path.join(FP_LIB, lib + ".pretty"), name)
    fp.SetReference(ref)
    fp.SetValue(value)
    fp.SetPosition(pt(x, y))
    fp.SetOrientationDegrees(rot)
    fp.Value().SetVisible(False)
    board.Add(fp)
    for pad in fp.Pads():
        if pad_nets and pad.GetNumber() in pad_nets:
            pad.SetNet(net(pad_nets[pad.GetNumber()]))
    return fp


def track(net_name, layer, width, *points):
    for (x1, y1), (x2, y2) in zip(points, points[1:]):
        t = pcbnew.PCB_TRACK(board)
        t.SetStart(pt(x1, y1))
        t.SetEnd(pt(x2, y2))
        t.SetWidth(mm(width))
        t.SetLayer(layer)
        t.SetNet(net(net_name))
        board.Add(t)


def line(layer, width, *points):
    for (x1, y1), (x2, y2) in zip(points, points[1:]):
        s = pcbnew.PCB_SHAPE(board)
        s.SetShape(pcbnew.SHAPE_T_SEGMENT)
        s.SetStart(pt(x1, y1))
        s.SetEnd(pt(x2, y2))
        s.SetLayer(layer)
        s.SetWidth(mm(width))
        board.Add(s)


def rect(layer, width, x1, y1, x2, y2):
    line(layer, width, (x1, y1), (x2, y1), (x2, y2), (x1, y2), (x1, y1))


def text(s, x, y, size=1.0, layer=pcbnew.F_SilkS, rot=0, bold=False):
    t = pcbnew.PCB_TEXT(board)
    t.SetText(s)
    t.SetPosition(pt(x, y))
    t.SetLayer(layer)
    t.SetTextSize(pcbnew.VECTOR2I(mm(size), mm(size)))
    t.SetTextThickness(mm(size * (0.2 if bold else 0.15)))
    t.SetTextAngleDegrees(rot)
    board.Add(t)


# ---- Outline, slot, mounting holes ----------------------------------------------
rect(pcbnew.Edge_Cuts, 0.1, 0, 0, BOARD_W, BOARD_H)
rect(pcbnew.Edge_Cuts, 0.1, *SLOT)
for i, (hx, hy) in enumerate([(3.5, 3.5), (BOARD_W - 3.5, 3.5),
                              (3.5, BOARD_H - 3.5), (BOARD_W - 3.5, BOARD_H - 3.5)]):
    h = footprint("MountingHole", "MountingHole_3.2mm_M3", f"H{i + 1}", "M3", hx, hy)
    h.Reference().SetVisible(False)

# ---- ESP32 sockets --------------------------------------------------------------
esp_nets_left = {"4": "SOIL", "14": "GND"}
esp_nets_right = {"2": "SCL", "5": "SDA", "11": "DHT_DATA", "14": "GND", "15": "+3V3"}
footprint("Connector_PinSocket_2.54mm", "PinSocket_1x15_P2.54mm_Vertical",
          "J1", "ESP32 left", ESP_LEFT_X, ESP_TOP_Y, pad_nets=esp_nets_left)
footprint("Connector_PinSocket_2.54mm", "PinSocket_1x15_P2.54mm_Vertical",
          "J2", "ESP32 right", ESP_RIGHT_X, ESP_TOP_Y, pad_nets=esp_nets_right)


def esp_y(pin):  # 1-based pin number on either row
    return ESP_TOP_Y + (pin - 1) * PITCH


# ---- Sensor sockets (rotated so the pins run left to right) ---------------------
# Rotation 90 makes the footprint's pin column (+y) run along +x.
bh_x0 = SENSOR_CX - 2 * PITCH
footprint("Connector_PinSocket_2.54mm", "PinSocket_1x05_P2.54mm_Vertical",
          "J3", "BH1750 (GY-302)", bh_x0, BH_Y, rot=90,
          pad_nets={"1": "+3V3", "2": "SCL", "3": "SDA", "4": "GND", "5": "GND"})
# Same pins again as plain solder holes, for wiring the sensor off-board.
footprint("Connector_PinHeader_2.54mm", "PinHeader_1x05_P2.54mm_Vertical",
          "J7", "BH1750 solder holes", bh_x0, BH_WIRE_Y, rot=90,
          pad_nets={"1": "+3V3", "2": "SCL", "3": "SDA", "4": "GND", "5": "GND"})
for ref in ("J3", "J7"):
    board.FindFootprintByReference(ref).Reference().SetVisible(False)
dht_x0 = SENSOR_CX - PITCH
footprint("Connector_PinSocket_2.54mm", "PinSocket_1x03_P2.54mm_Vertical",
          "J4", "DHT22 module", dht_x0, DHT_Y, rot=90,
          pad_nets={"1": "DHT_DATA", "2": "+3V3", "3": "GND"})

# ---- Soil sensor: header + solder holes (same pin order as the HW-390) ----------
soil_nets = {"1": "SOIL", "2": "+3V3", "3": "GND"}
footprint("Connector_PinHeader_2.54mm", "PinHeader_1x03_P2.54mm_Vertical",
          "J5", "Soil header", SOIL_HDR_X, SOIL_Y, pad_nets=soil_nets)
footprint("Connector_PinHeader_2.54mm", "PinHeader_1x03_P2.54mm_Vertical",
          "J6", "Soil solder holes", SOIL_PAD_X, SOIL_Y, pad_nets=soil_nets)
for ref in ("J5", "J6"):
    board.FindFootprintByReference(ref).Reference().SetVisible(False)

# ---- Antenna keep-out: no copper under the ESP32 antenna -------------------------
keepout = pcbnew.ZONE(board)
keepout.SetIsRuleArea(True)
keepout.SetDoNotAllowTracks(True)
keepout.SetDoNotAllowVias(True)
keepout.SetDoNotAllowCopperPour(True)
keepout.SetDoNotAllowPads(True)
keepout.SetDoNotAllowFootprints(False)
ls = pcbnew.LSET()
ls.AddLayer(pcbnew.F_Cu)
ls.AddLayer(pcbnew.B_Cu)
keepout.SetLayerSet(ls)
kx1, kx2 = ESP_LEFT_X - 3.5, ESP_RIGHT_X + 3.5
ky2 = ESP_TOP_Y - 2.5
outline = keepout.Outline()
outline.NewOutline()
for x, y in [(kx1, 0), (kx2, 0), (kx2, ky2), (kx1, ky2)]:
    outline.Append(mm(ORIGIN[0] + x), mm(ORIGIN[1] + y))
board.Add(keepout)

# ---- Tracks -------------------------------------------------------------------
F, B = pcbnew.F_Cu, pcbnew.B_Cu
xr = ESP_RIGHT_X
soil_vcc_y, soil_gnd_y = SOIL_Y + PITCH, SOIL_Y + 2 * PITCH
bus_y = 53.0      # 3V3 bus (bottom layer), below the ESP32 pins
gnd_y = 59.0      # GND return (top layer), along the bottom edge
gnd_x = 64.0      # GND riser (top layer), right side
bh = {n: bh_x0 + (n - 1) * PITCH for n in range(1, 6)}
dht = {n: dht_x0 + (n - 1) * PITCH for n in range(1, 4)}

# Soil: D34 -> solder holes -> header
track("SOIL", F, SIGNAL_W, (ESP_LEFT_X, esp_y(4)), (SOIL_HDR_X, SOIL_Y))
# Soil VCC / GND: link header to holes, then to the power nets
track("+3V3", F, POWER_W, (SOIL_HDR_X, soil_vcc_y), (SOIL_PAD_X, soil_vcc_y))
track("GND", F, POWER_W, (SOIL_HDR_X, soil_gnd_y), (SOIL_PAD_X, soil_gnd_y))
track("+3V3", B, POWER_W, (SOIL_PAD_X, soil_vcc_y), (7.55, soil_vcc_y + 1.95),
      (7.55, bus_y), (xr, bus_y))
track("GND", F, POWER_W, (SOIL_PAD_X, soil_gnd_y), (7.0, soil_gnd_y + 1.4),
      (7.0, esp_y(14) - 1.5), (ESP_LEFT_X - 1.0, esp_y(14)), (ESP_LEFT_X, esp_y(14)))
# Tie both ESP32 GND pins together (so the board is complete without the ESP32)
track("GND", F, POWER_W, (ESP_LEFT_X, esp_y(14)), (xr, esp_y(14)))

# 3V3: ESP32 3V3 pin -> bus -> DHT22 + and BH1750 VCC
track("+3V3", B, POWER_W, (xr, esp_y(15)), (xr, bus_y))
track("+3V3", B, POWER_W, (xr, bus_y), (48.0, bus_y), (dht[2], bus_y), (dht[2], DHT_Y))
track("+3V3", B, POWER_W, (48.0, bus_y), (48.0, BH_WIRE_Y + 2.4), (bh[1], BH_WIRE_Y),
      (bh[1], BH_Y))

# GND: ESP32 GND -> bottom edge -> right riser -> DHT22 -, BH1750 GND + ADDR
track("GND", F, POWER_W, (xr, esp_y(14)), (36.5, esp_y(14) + 1.6), (36.5, gnd_y),
      (gnd_x, gnd_y), (gnd_x, 5.0), (bh[4], 5.0), (bh[4], BH_Y))
track("GND", F, POWER_W, (bh[5], 5.0), (bh[5], BH_Y))
track("GND", F, POWER_W, (bh[4], BH_Y), (bh[4], BH_WIRE_Y))
track("GND", F, POWER_W, (bh[5], BH_Y), (bh[5], BH_WIRE_Y))
track("GND", F, POWER_W, (gnd_x, DHT_Y), (dht[3], DHT_Y))

# I2C: D22 = SCL, D21 = SDA -> BH1750 (top layer, above the slot)
track("SCL", F, SIGNAL_W, (xr, esp_y(2)), (bh[2] - 2.3, esp_y(2)),
      (bh[2], esp_y(2) - 2.3), (bh[2], BH_Y))
track("SDA", F, SIGNAL_W, (xr, esp_y(5)), (bh[3], esp_y(5)), (bh[3], BH_Y))

# DHT22 data: D4 -> below the slot -> DHT22 OUT
track("DHT_DATA", F, SIGNAL_W, (xr, esp_y(11)), (37.5, esp_y(11) + 2.6), (37.5, 45.5),
      (dht[1] - 2.5, 45.5), (dht[1], 48.0), (dht[1], DHT_Y))

# ---- Silkscreen ------------------------------------------------------------------
S = pcbnew.F_SilkS
# ESP32 outline and orientation
ex1 = (ESP_LEFT_X + ESP_RIGHT_X) / 2 - ESP_W / 2
ey1 = ESP_TOP_Y - ESP_PIN1_FROM_TOP
# Top and bottom edges only: the pin sockets already mark the long sides.
ex2, ey2 = ex1 + ESP_W, ey1 + ESP_L
line(S, 0.15, (ex1, ey1 + 3), (ex1, ey1), (ex2, ey1), (ex2, ey1 + 3))
line(S, 0.15, (ex1, ey2 - 1), (ex1, ey2), (ex2, ey2), (ex2, ey2 - 1))
text("ANTENNA - no copper", (ESP_LEFT_X + ESP_RIGHT_X) / 2, ey1 + 3.0, 1.2)
text("ESP32 DevKit V1 - 30 pin", (ESP_LEFT_X + ESP_RIGHT_X) / 2, ey1 + 6.0, 1.2, bold=True)
text("USB", (ESP_LEFT_X + ESP_RIGHT_X) / 2, ey1 + ESP_L - 2.0, 1.2, bold=True)
for i, (l, r) in enumerate(zip(ESP_LEFT, ESP_RIGHT)):
    y = esp_y(i + 1)
    text(l, ESP_LEFT_X + 3.0, y, 0.9)
    text(r, ESP_RIGHT_X - 3.0, y, 0.9)

# Sensor modules (outline shows where the module lies; match the pin labels)
for n, lab in enumerate(["VCC", "SCL", "SDA", "GND", "ADDR"], 1):
    text(lab, bh[n], BH_Y - 3.6, 0.8, rot=90)
text("socket", bh[1] - 3.6, BH_Y, 0.8)
text("wires", bh[1] - 3.6, BH_WIRE_Y, 0.8)
text("BH1750 / GY-302", SENSOR_CX, BH_WIRE_Y + 3.8, 1.0, bold=True)
text("use one row", SENSOR_CX, BH_WIRE_Y + 5.8, 0.8)
for n, lab in enumerate(["DAT", "VCC", "GND"], 1):
    text(lab, dht[n], DHT_Y + 3.6, 0.8, rot=90)
text("DHT22 module", SENSOR_CX, DHT_Y - 4.0, 1.0, bold=True)

# Soil connectors
text("SOIL", (SOIL_HDR_X + SOIL_PAD_X) / 2, SOIL_Y - 2.6, 1.0, bold=True)
for n, lab in enumerate(["A", "V", "G"]):   # AOUT, VCC, GND as on the HW-390
    text(lab, SOIL_PAD_X + 2.0, SOIL_Y + n * PITCH, 0.8)
text("hdr", SOIL_HDR_X, SOIL_Y + 3 * PITCH + 0.2, 0.8)
text("wire", SOIL_PAD_X + 0.4, SOIL_Y + 3 * PITCH + 0.2, 0.8)
text("use one", (SOIL_HDR_X + SOIL_PAD_X) / 2 + 0.3, SOIL_Y + 4.3 * PITCH, 0.8)

text("plantMonitor carrier v1", SENSOR_CX, 33.5, 1.0, bold=True)
text("2026-10", SENSOR_CX, 35.5, 0.8)

# ---- Design rules (fit JLCPCB/PCBWay standard 2-layer) ---------------------------
ds = board.GetDesignSettings()
ds.m_MinClearance = mm(0.2)
ds.m_TrackMinWidth = mm(0.2)
ds.m_CopperEdgeClearance = mm(0.3)
ds.m_HolesMinimumClearance = mm(0.25)

board.BuildConnectivity()
pcbnew.SaveBoard(OUT, board)
print("wrote", OUT)
for fp in board.GetFootprints():
    for pad in fp.Pads():
        if pad.GetNetname():
            p = pad.GetPosition()
            print(f"  {fp.GetReference()}.{pad.GetNumber():>2} {pad.GetNetname():9} "
                  f"({pcbnew.ToMM(p.x) - ORIGIN[0]:6.2f}, {pcbnew.ToMM(p.y) - ORIGIN[1]:6.2f})")
