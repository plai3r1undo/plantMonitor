#pragma once

// ---- Which sensors are connected (1 = yes, 0 = no) -----------------------
// A sensor set to 0 is reported as "no value" instead of a bogus reading
// (an unconnected soil pin reads random noise).
#define HAS_DHT22 1
#define HAS_BH1750 1
#define HAS_SOIL_SENSOR 1

// ---- Wiring --------------------------------------------------------------
// DHT22 data pin (add a 10k pull-up to 3V3 if your module doesn't have one)
#define DHT_PIN 4
// BH1750 on I2C (ADDR pin low/floating = 0x23, high = 0x5C)
#define I2C_SDA_PIN 21
#define I2C_SCL_PIN 22
#define BH1750_ADDRESS 0x23
// HW-390 analog out. Must be an ADC1 pin (GPIO 32-39): ADC2 is unusable
// while WiFi is on.
#define SOIL_PIN 34
// Optional GPIO that powers the sensors only while measuring (saves power
// on battery). -1 = sensors are wired to 3V3 permanently.
#define SENSOR_POWER_PIN -1

// ---- Behaviour -----------------------------------------------------------
// Seconds of deep sleep between measurements
#define MEASURE_INTERVAL_S 600
// ADC samples averaged per soil reading
#define SOIL_SAMPLES 16
#define WIFI_TIMEOUT_MS 15000
#define HTTP_TIMEOUT_MS 5000
#define HTTP_RETRIES 3
