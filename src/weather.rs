use serde::Deserialize;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(2 * 60);

#[derive(Debug, Clone)]
pub struct WeatherData {
    pub temperature: f64,
    pub apparent_temperature: f64,
    pub weather_code: u16,
    pub wind_speed: f64,
    pub high_temperature: f64,
    pub low_temperature: f64,
    pub fetched_at: Instant,
}

impl WeatherData {
    pub fn description(&self) -> &'static str {
        weather_description(self.weather_code)
    }
}

pub type WeatherUpdate = Result<WeatherData, String>;

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    current: CurrentWeather,
    daily: DailyWeather,
}

#[derive(Debug, Deserialize)]
struct CurrentWeather {
    temperature_2m: f64,
    apparent_temperature: f64,
    weather_code: u16,
    wind_speed_10m: f64,
}

#[derive(Debug, Deserialize)]
struct DailyWeather {
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
}

pub fn spawn_worker(latitude: f64, longitude: f64) -> Receiver<WeatherUpdate> {
    let (updates_tx, updates_rx) = mpsc::channel();
    thread::spawn(move || {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(8))
            .build();
        loop {
            let update = fetch_weather(&agent, latitude, longitude);
            let delay = if update.is_ok() {
                REFRESH_INTERVAL
            } else {
                RETRY_INTERVAL
            };
            if updates_tx.send(update).is_err() {
                break;
            }
            thread::sleep(delay);
        }
    });
    updates_rx
}

fn fetch_weather(agent: &ureq::Agent, latitude: f64, longitude: f64) -> WeatherUpdate {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={latitude}&longitude={longitude}\
         &current=temperature_2m,apparent_temperature,weather_code,wind_speed_10m\
         &daily=temperature_2m_max,temperature_2m_min&timezone=auto&forecast_days=1"
    );
    let response = agent
        .get(&url)
        .call()
        .map_err(|error| format!("weather request failed: {error}"))?;
    let forecast: ForecastResponse = response
        .into_json()
        .map_err(|error| format!("invalid weather response: {error}"))?;

    let high_temperature = *forecast
        .daily
        .temperature_2m_max
        .first()
        .ok_or_else(|| "weather response omitted today's high".to_string())?;
    let low_temperature = *forecast
        .daily
        .temperature_2m_min
        .first()
        .ok_or_else(|| "weather response omitted today's low".to_string())?;

    Ok(WeatherData {
        temperature: forecast.current.temperature_2m,
        apparent_temperature: forecast.current.apparent_temperature,
        weather_code: forecast.current.weather_code,
        wind_speed: forecast.current.wind_speed_10m,
        high_temperature,
        low_temperature,
        fetched_at: Instant::now(),
    })
}

fn weather_description(code: u16) -> &'static str {
    match code {
        0 => "Clear",
        1 => "Mostly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Foggy",
        51 | 53 | 55 => "Drizzle",
        56 | 57 => "Freezing drizzle",
        61 | 63 | 65 => "Rain",
        66 | 67 => "Freezing rain",
        71 | 73 | 75 | 77 => "Snow",
        80..=82 => "Rain showers",
        85 | 86 => "Snow showers",
        95 => "Thunderstorm",
        96 | 99 => "Thunder + hail",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_open_meteo_forecast_fields() {
        let json = r#"{
            "current": {
                "temperature_2m": 17.4,
                "apparent_temperature": 16.2,
                "weather_code": 2,
                "wind_speed_10m": 12.1
            },
            "daily": {
                "temperature_2m_max": [20.5],
                "temperature_2m_min": [11.3]
            }
        }"#;
        let forecast: ForecastResponse = serde_json::from_str(json).unwrap();

        assert_eq!(forecast.current.temperature_2m, 17.4);
        assert_eq!(forecast.current.weather_code, 2);
        assert_eq!(forecast.daily.temperature_2m_max, vec![20.5]);
        assert_eq!(weather_description(2), "Partly cloudy");
    }
}
