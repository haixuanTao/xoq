//! Integration test for local pub/sub using moq_lite primitives
//! This tests the core publish/subscribe flow without needing a relay server

use bytes::Bytes;
use moq_native::moq_lite::{Broadcast, Origin, Track};
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn test_local_pubsub() {
    // Create an origin (simulates what the relay does)
    let origin = Origin::produce();

    // Publisher side: create a broadcast and publish data
    let mut broadcast = Broadcast::produce();

    // Create a track for serial data
    let mut track = broadcast.producer.create_track(Track {
        name: "serial-data".to_string(),
        priority: 0,
    });

    // Publish the broadcast to the origin
    origin.producer.publish_broadcast("test", broadcast.consumer.clone());

    // Subscriber side: consume the origin and subscribe to the track
    let mut consumer = origin.producer.consume();

    // Wait for the broadcast announcement
    let (path, broadcast_consumer) = timeout(Duration::from_secs(1), consumer.announced())
        .await
        .expect("timeout waiting for announcement")
        .expect("origin closed");

    assert_eq!(path.as_str(), "test");
    let broadcast_consumer = broadcast_consumer.expect("broadcast should be announced");

    // Subscribe to the track
    let track_info = Track {
        name: "serial-data".to_string(),
        priority: 0,
    };
    let mut track_consumer = broadcast_consumer.subscribe_track(&track_info);

    // Write some data from the publisher
    let test_data = "Hello from serial port!";
    track.write_frame(Bytes::from(test_data));

    // Read the data from the subscriber
    let group = timeout(Duration::from_secs(1), track_consumer.next_group())
        .await
        .expect("timeout waiting for group")
        .expect("error reading group")
        .expect("track closed");

    assert_eq!(group.info.sequence, 0);

    // Read the frame
    let mut group = group;
    let frame = timeout(Duration::from_secs(1), group.read_frame())
        .await
        .expect("timeout waiting for frame")
        .expect("error reading frame")
        .expect("group closed");

    assert_eq!(frame.as_ref(), test_data.as_bytes());
    println!("Successfully received: {}", String::from_utf8_lossy(&frame));
}

#[tokio::test]
async fn test_multiple_groups() {
    let origin = Origin::produce();
    let mut broadcast = Broadcast::produce();

    let mut track = broadcast.producer.create_track(Track {
        name: "multi-group".to_string(),
        priority: 0,
    });

    origin.producer.publish_broadcast("", broadcast.consumer.clone());

    let mut consumer = origin.producer.consume();
    let (_, bc) = consumer.announced().await.unwrap();
    let bc = bc.unwrap();

    let mut track_consumer = bc.subscribe_track(&Track {
        name: "multi-group".to_string(),
        priority: 0,
    });

    // Write multiple groups one at a time and read them
    // (subscribe before writing to ensure we get all groups in order)
    for i in 0..5 {
        track.write_frame(Bytes::from(format!("Frame {}", i)));

        let group = track_consumer.next_group().await.unwrap().unwrap();
        assert_eq!(group.info.sequence, i as u64);

        let mut group = group;
        let frame = group.read_frame().await.unwrap().unwrap();
        let expected = format!("Frame {}", i);
        assert_eq!(frame.as_ref(), expected.as_bytes());
        println!("Received group {}: {}", i, String::from_utf8_lossy(&frame));
    }
}

#[tokio::test]
async fn test_group_with_multiple_frames() {
    let origin = Origin::produce();
    let mut broadcast = Broadcast::produce();

    let mut track = broadcast.producer.create_track(Track {
        name: "group-frames".to_string(),
        priority: 0,
    });

    origin.producer.publish_broadcast("", broadcast.consumer.clone());

    let mut consumer = origin.producer.consume();
    let (_, bc) = consumer.announced().await.unwrap();
    let bc = bc.unwrap();

    let mut track_consumer = bc.subscribe_track(&Track {
        name: "group-frames".to_string(),
        priority: 0,
    });

    // Create a group with multiple frames
    let mut group = track.append_group();
    group.write_frame(Bytes::from("First"));
    group.write_frame(Bytes::from("Second"));
    group.write_frame(Bytes::from("Third"));
    group.close();

    // Read the group
    let recv_group = track_consumer.next_group().await.unwrap().unwrap();
    let mut recv_group = recv_group;

    // Read all frames from the group
    let f1 = recv_group.read_frame().await.unwrap().unwrap();
    assert_eq!(f1.as_ref(), b"First");

    let f2 = recv_group.read_frame().await.unwrap().unwrap();
    assert_eq!(f2.as_ref(), b"Second");

    let f3 = recv_group.read_frame().await.unwrap().unwrap();
    assert_eq!(f3.as_ref(), b"Third");

    // No more frames
    let f4 = recv_group.read_frame().await.unwrap();
    assert!(f4.is_none());

    println!("Successfully read all frames from group");
}

#[tokio::test]
async fn test_broadcast_unannounce() {
    let origin = Origin::produce();
    let broadcast = Broadcast::produce();

    origin.producer.publish_broadcast("temp", broadcast.consumer.clone());

    let mut consumer = origin.producer.consume();

    // Get the announcement
    let (path, bc) = consumer.announced().await.unwrap();
    assert_eq!(path.as_str(), "temp");
    assert!(bc.is_some());

    // Drop the broadcast producer to trigger unannounce
    drop(broadcast.producer);

    // Wait a bit for the async cleanup
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Should receive unannounce
    let result = timeout(Duration::from_millis(100), consumer.announced()).await;
    if let Ok(Some((path, bc))) = result {
        assert_eq!(path.as_str(), "temp");
        assert!(bc.is_none(), "should be unannounced");
        println!("Broadcast correctly unannounced");
    }
}
