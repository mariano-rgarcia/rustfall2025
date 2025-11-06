use serde::Deserialize;
use std::error::Error;
use std::fs::File;
use std::io::{Read, Write};

#[derive(Debug, Deserialize)]
struct DogImage {
    message: String,
    status: String,
}

#[derive(Debug)]
enum FetchError {
    Api(String),
    Network(String),
    Download(String),
    Save(String),
    Parse(String),
}

fn fetch_random_dog_image() -> Result<DogImage, FetchError> {
    let url = "https://dog.ceo/api/breeds/image/random";
    match ureq::get(url).call() {
        Ok(response) => {
            if response.status() == 200 {
                match response.into_json::<DogImage>() {
                    Ok(dog_image) => Ok(dog_image),
                    Err(e) => Err(FetchError::Parse(format!("Failed to parse JSON: {}", e))),
                }
            } else {
                Err(FetchError::Api(format!("HTTP error: {}", response.status())))
            }
        }
        Err(e) => Err(FetchError::Network(format!("Request failed: {}", e))),
    }
}

fn download_image(url: &str, filename: &str) -> Result<(), FetchError> {
    match ureq::get(url).call() {
        Ok(mut response) => {
            let mut image_bytes = Vec::new();
            if let Err(e) = response.into_reader().read_to_end(&mut image_bytes) {
                return Err(FetchError::Download(format!("Failed to read bytes: {}", e)));
            }
            let mut file = match File::create(filename) {
                Ok(f) => f,
                Err(e) => return Err(FetchError::Save(format!("Failed to create file: {}", e))),
            };
            if let Err(e) = file.write_all(&image_bytes) {
                return Err(FetchError::Save(format!("Failed to write file: {}", e)));
            }
            Ok(())
        }
        Err(e) => Err(FetchError::Download(format!("Failed to fetch image: {}", e))),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("Dog Image Fetcher");
    println!("=================\n");

    for i in 1..=5 {
        println!("Fetching random dog image #{}", i);
        match fetch_random_dog_image() {
            Ok(dog_image) => {
                println!("✅ Success!");
                println!("🖼️ Image URL: {}", dog_image.message);
                let filename = format!("dog_image_{}.jpg", i);
                match download_image(&dog_image.message, &filename) {
                    Ok(()) => println!("📥 Downloaded and saved as: {}", filename),
                    Err(e) => println!("❌ Download/Save Error: {:?}", e),
                }
            }
            Err(e) => println!("❌ Error: {:?}", e),
        }
        println!();
    }

    Ok(())
}
