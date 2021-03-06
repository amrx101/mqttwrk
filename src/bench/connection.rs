use std::io;
use std::sync::Arc;
use std::time::Instant;


use crate::BenchConfig;
use hdrhistogram::Histogram;
use crate::bench::Status;

use crate::link::Link;
use rumqttc::*;
use thiserror::Error;
use tokio::sync::Barrier;
use tokio::time::Duration;
use tokio::{pin, select, task, time};

pub(crate) struct Connection {
    config: Arc<BenchConfig>,
    link: Link,
}

#[derive(Error, Debug)]
pub enum ConnectionError {
    #[error("IO error = {0:?}")]
    Io(#[from] io::Error),
    #[error("Connection error = {0:?}")]
    Connection(#[from] rumqttc::ConnectionError),
    #[error("Wrong packet = {0:?}")]
    WrongPacket(Incoming),
}

impl Connection {
    pub async fn new(
        mut link: Link,
        config: Arc<BenchConfig>,
    ) -> Result<Connection, ConnectionError> {
        let client = link.client.clone();
        let subscribers = config.subscribers;
        let publishers = config.publishers;
        let connections = config.connections;
        let qos = config.qos;

        // If there are 10 connections with 3 publishers and 2 subscribers,
        // each connection publishes to topics 'hello/rumqtt-{id}/{0, 1, 2}/world'
        // and subscribes to all the topics of first 2 connections. 'hello/rumqtt-{0, 1}/+/world'
        // 2 subscribers implies, every connection subscribes to all the topics
        // for first 2 connections
        task::spawn(async move {
            if subscribers > 0 {
                assert!(publishers > 0, "There should be at least 1 publisher");
                assert!(
                    connections >= subscribers,
                    "#connections should be > subscribers"
                );
            }

            for i in 0..subscribers {
                let topic = format!("hello/rumqtt-{:05}/+/world", i);
                client.subscribe(topic, get_qos(qos)).await.unwrap();
            }
        });

        // Handle connection and subscriptions first
        let mut sub_ack_count = 0;
        loop {
            let event = link.eventloop.poll().await?;
            if let Event::Incoming(v) = event {
                match v {
                    Incoming::SubAck(_) => sub_ack_count += 1,
                    Incoming::ConnAck(_) => (),
                    incoming => return Err(ConnectionError::WrongPacket(incoming)),
                }
            }

            if sub_ack_count >= subscribers {
                break;
            }
        }

        Ok(Connection { config, link })
    }

    pub async fn start(&mut self, barrier: Arc<Barrier>) {
        let start = Instant::now();

        // Wait for all the subscription from other connections to finish
        // while doing ping requests so that broker doesn't disconnect
        let barrier = barrier.wait();
        pin!(barrier);

        // println!("await barrier = {:?}", self.id);
        loop {
            select! {
                _ = self.link.eventloop.poll() => {},
                _ = &mut barrier => break,
            }
        }

        // println!("done barrier = {:?}", self.id);
        if self.link.id == "rumqtt-00000" {
            println!(
                "Connections & subscriptions ok. Elapsed = {:?}",
                start.elapsed().as_secs()
            );
        }

        let qos = get_qos(self.config.qos);
        let payload_size = self.config.payload_size;
        let count = self.config.count;
        let publishers = self.config.publishers;
        let subscribers = self.config.subscribers;
        let delay = self.config.delay;
        let id = self.link.id.clone();

        let start = Instant::now();
        let acks_expected = count * publishers;
        let incoming_expected = count * publishers * subscribers;
        let mut outgoing_elapsed = Duration::from_secs(0);
        let mut incoming_elapsed = Duration::from_secs(0);
        let mut outgoing_done = false;
        let mut incoming_done = false;
        let mut acks_count = 0;
        let mut incoming_count = 0;
        let mut hist = Histogram::<u64>::new(4).unwrap();

        let size = self.config.max_inflight as usize;
        let mut time_vec: Vec<Option<std::time::Instant>> = vec![None; size+1];
        let p_sender = &self.link.sender;
        

        for i in 0..publishers {
            let topic = format!("hello/{}/{}/world", self.link.id, i);
            let client = self.link.client.clone();
            task::spawn(async move {
                requests(topic, payload_size, count, client, qos, delay).await;
            });
        }

        let mut increment_cnt = 0;

        let mut reconnects: i32 = 0;
        let mut interval = time::interval(time::Duration::from_secs(1));
        loop {
            tokio::select!{
                event = self.link.eventloop.poll() => {
                    let message = match event {
                        Ok(v) => v,
                        Err(e) => {
                            error!("Id = {}, Connection error = {:?}", self.link.id, e);
                            reconnects += 1;
                            if reconnects == 1 {
                                break;
                            }
        
                            continue;
                        }
                    };
                    if self.config.publishers == 0 || self.config.count == 0 {
                        continue;
                    }
                    match message {
                        Event::Incoming(v) => {
                            match v {
                                Incoming::PubAck(pkid) => {
                                    acks_count += 1;
                                    // PubACK received for pkid `x`. Server acknowledged it. 
                                    let index =pkid.pkid as usize;
        
                                    match time_vec[index] {
                                        Some(v) => {
                                            let time_elapsed = Instant::now().duration_since(v).as_millis();
                                            hist.record(time_elapsed as u64).unwrap();
                                            time_vec[index] = None
                                        },
                                        None => warn!("No publish record for Pkid={:?}", index),
                                    };
                                    increment_cnt += 1;
                                },
                                Incoming::Publish(_publish) => {
                                    incoming_count += 1;
                                },
                                Incoming::PingResp => {}
                                incoming => {
                                    error!("Id = {}, Unexpected incoming packet = {:?}", id, incoming);
                                    break;
                                }
                            }
                        },
                        Event::Outgoing(v) =>{
                            match v {
                                Outgoing::Publish(_pkid) => {
                                    // we are trying to send out a package with pkid `x`.
                                    let index = _pkid as usize;
                                    time_vec.insert(index, Some(Instant::now()));
                                },
                                _ => {},
                            }
                        },
                    }
                },
                _ = interval.tick() => {
                    let msg = Status::Increment(increment_cnt);
                    p_sender.send(msg).await.unwrap();
                    increment_cnt = 0;
                }
            }

            if !outgoing_done && acks_count >= acks_expected {
                outgoing_elapsed = start.elapsed();
                outgoing_done = true;
            }

            if !incoming_done && incoming_count >= incoming_expected {
                incoming_elapsed = start.elapsed();
                incoming_done = true;
            }

            if outgoing_done && incoming_done {
                break;
            }
        }

        let outgoing_throughput = (acks_count * 1000) as f32 / outgoing_elapsed.as_millis() as f32;
        let incoming_throughput =
            (incoming_count * 1000) as f32 / incoming_elapsed.as_millis() as f32;

        println!(
            "Id = {}
            Outgoing publishes : Received = {:<7} Throughput = {} messages/s
            Incoming publishes : Received = {:<7} Throughput = {} messages/s
            Reconnects         : {}",
            self.link.id,
            acks_count,
            outgoing_throughput,
            incoming_count,
            incoming_throughput,
            reconnects,
        );
        println!("# of samples          : {}", hist.len());
        println!(
            "99.999'th percentile  : {}",
            hist.value_at_quantile(0.999999)
        );
        println!(
            "99.99'th percentile   : {}",
            hist.value_at_quantile(0.99999)
        );
        println!("90 percentile         : {}", hist.value_at_quantile(0.90));
        println!("50 percentile         : {}", hist.value_at_quantile(0.5));

        let msg = Status::Hist(hist);

        // send the histogram over this channel
        p_sender.send(msg).await.unwrap();

    }
}

/// make count number of requests at specified QoS.
async fn requests(
    topic: String,
    payload_size: usize,
    count: usize,
    client: AsyncClient,
    qos: QoS,
    delay: u64,
) {
    let mut interval = match delay {
        0 => None,
        delay => Some(time::interval(time::Duration::from_secs(delay))),
    };

    for _i in 0..count {
        let payload = vec![0; payload_size];
        if let Some(interval) = &mut interval {
            interval.tick().await;
        }

        // These errors are usually due to eventloop task being dead. We can ignore the
        // error here as the failed eventloop task would have already printed an error
        if let Err(_e) = client.publish(topic.clone(), qos, false, payload).await {
            break;
        }
    }
}

/// get QoS level. Default is AtLeastOnce.
fn get_qos(qos: i16) -> QoS {
    match qos {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => QoS::AtLeastOnce,
    }
}
