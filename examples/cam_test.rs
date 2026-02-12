use objc2::class;
use objc2::msg_send;
use objc2::runtime::Bool;
use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeVideo};

fn main() {
    eprintln!("Testing AVFoundation from main thread...");

    unsafe {
        let media_type = AVMediaTypeVideo.expect("AVMediaTypeVideo not available");

        eprintln!("Checking auth status...");
        let status: isize = msg_send![
            class!(AVCaptureDevice),
            authorizationStatusForMediaType: media_type
        ];
        eprintln!("Auth status: {} (3=authorized)", status);

        eprintln!("Calling defaultDeviceWithMediaType...");
        let device = AVCaptureDevice::defaultDeviceWithMediaType(media_type);
        match device {
            Some(dev) => {
                let name = dev.localizedName().to_string();
                eprintln!("Default device: {}", name);
            }
            None => eprintln!("No default camera found"),
        }

        eprintln!("Done!");
    }
}
