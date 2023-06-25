use itertools::Itertools;
use pyo3::exceptions::PyException;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyType};
use redis::Commands;
use redis::{Connection, RedisResult};
use std::collections::HashMap;
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;

// This could be completely wrong, not sure if it would break the channel, let's try 🤞
static REDIS_JOB_TX: OnceLock<Mutex<mpsc::Sender<RedisJob>>> = OnceLock::new();
const EXPIRE_KEY_SECONDS: usize = 3600;

#[derive(Debug)]
enum BackendAction {
    Inc,
    Dec,
    Set,
    Get,
}

#[derive(Debug)]
struct RedisJobResult {
    // value: f64,
    values: Vec<f64>,
}

struct RedisJob {
    action: BackendAction,
    key_name: String,
    labels_hash: Option<String>,
    value: f64,
    result_tx: Option<mpsc::Sender<RedisJobResult>>,
    pipeline: Option<redis::Pipeline>,
}

#[derive(Debug)]
#[pyclass]
struct RedisBackend {
    #[pyo3(get)]
    config: Py<PyDict>,
    #[pyo3(get)]
    metric: Py<PyAny>,
    #[pyo3(get)]
    histogram_bucket: Option<String>,
    redis_job_tx: mpsc::Sender<RedisJob>,
    key_name: String,
    labels_hash: Option<String>,
}

// Sample(suffix='_bucket', labels={'le': '0.005'}, value=0.0
#[derive(Debug, FromPyObject)]
struct Sample<'a> {
    suffix: String,
    labels: Option<HashMap<String, String>>,
    // value: f64,
    value: PyRef<'a, RedisBackend>,
}

#[derive(Debug)]
#[pyclass]
struct OutSample {
    #[pyo3(get)]
    suffix: String,
    #[pyo3(get)]
    labels: Option<HashMap<String, String>>,
    #[pyo3(get)]
    value: f64,
}

impl OutSample {
    fn new(suffix: String, labels: Option<HashMap<String, String>>, value: f64) -> Self {
        Self {
            suffix,
            labels,
            value,
        }
    }
}

#[derive(Debug)]
struct SamplesResultDict {
    collectors: Vec<Py<PyAny>>,
    samples_vec: Vec<Vec<OutSample>>,
}

impl SamplesResultDict {
    fn new() -> Self {
        Self {
            collectors: vec![],
            samples_vec: vec![],
        }
    }
}

impl IntoPy<PyResult<PyObject>> for SamplesResultDict {
    fn into_py(self, py: Python<'_>) -> PyResult<PyObject> {
        let pydict = PyDict::new(py);
        for (collector, samples) in self
            .collectors
            .into_iter()
            .zip(self.samples_vec.into_iter())
        {
            pydict.set_item(collector, samples.into_py(py))?;
        }
        Ok(pydict.into())
    }
}

fn create_redis_connection(host: &str, port: u16) -> RedisResult<Connection> {
    let url = format!("redis://{host}:{port}");
    let client = redis::Client::open(url)?;
    let con = client.get_connection()?;
    Ok(con)
}

#[pymethods]
impl RedisBackend {
    #[new]
    fn new(config: &PyDict, metric: &PyAny, histogram_bucket: Option<String>) -> PyResult<Self> {
        // producer
        let redis_job_tx_mutex = REDIS_JOB_TX.get().unwrap();
        let redis_job_tx = redis_job_tx_mutex.lock().unwrap();
        let cloned_tx = redis_job_tx.clone();

        let py = metric.py();
        let collector = metric.getattr(intern!(metric.py(), "_collector"))?;

        let mut key_name: String = metric
            .getattr(intern!(py, "_collector"))?
            .getattr(intern!(py, "name"))?
            .extract()?;

        if let Some(bucket_id) = histogram_bucket.clone() {
            key_name = format!("{key_name}:{bucket_id}");
        }

        let mut default_labels: Option<HashMap<&str, &str>> = None;
        let mut metric_labels: Option<HashMap<&str, &str>> = None;

        let py_metric_labels = metric.getattr(intern!(py, "_labels"))?;
        if py_metric_labels.is_true()? {
            let labels: HashMap<&str, &str> = py_metric_labels.extract()?;
            metric_labels = Some(labels);
        }

        // default labels
        if collector
            .getattr(intern!(py, "_default_labels_count"))?
            .is_true()?
        {
            let labels: HashMap<&str, &str> = collector
                .getattr(intern!(py, "_default_labels"))?
                .extract()?;

            default_labels = Some(labels);
        }

        let to_hash = {
            if let Some(mut default_labels) = default_labels {
                if let Some(metric_labels) = metric_labels {
                    default_labels.extend(&metric_labels);
                }
                Some(default_labels)
            } else {
                metric_labels
            }
        };

        let labels_hash = to_hash.map(|labels| labels.values().sorted().join("-"));

        Ok(Self {
            config: config.into(),
            metric: metric.into(),
            histogram_bucket,
            redis_job_tx: cloned_tx,
            key_name,
            labels_hash,
        })
    }

    #[classmethod]
    fn _initialize(cls: &PyType, config: &PyDict) -> PyResult<()> {
        println!("hello: {}", cls);

        // using the PyAny::get_item so that it will raise a KeyError on missing key
        let host: &str = PyAny::get_item(config, intern!(config.py(), "host"))?.extract()?;
        let port: u16 = PyAny::get_item(config, intern!(config.py(), "port"))?.extract()?;

        let mut connection = match create_redis_connection(host, port) {
            Ok(connection) => connection,
            Err(e) => return Err(PyException::new_err(e.to_string())),
        };

        // producer / consumer
        let (tx, rx) = mpsc::channel();
        REDIS_JOB_TX.get_or_init(|| Mutex::new(tx));

        thread::spawn(move || {
            println!("In thread....");
            while let Ok(received) = rx.recv() {
                match received.action {
                    BackendAction::Inc | BackendAction::Dec => {
                        match received.labels_hash {
                            Some(labels_hash) => connection
                                .hincr(&received.key_name, &labels_hash, received.value)
                                .unwrap(),
                            None => connection.incr(&received.key_name, received.value).unwrap(),
                        }
                        let _: () = connection
                            .expire(&received.key_name, EXPIRE_KEY_SECONDS)
                            .unwrap();
                    }
                    BackendAction::Set => {
                        match received.labels_hash {
                            Some(labels_hash) => connection
                                .hset(&received.key_name, &labels_hash, received.value)
                                .unwrap(),
                            None => connection.set(&received.key_name, received.value).unwrap(),
                        }
                        let _: () = connection
                            .expire(&received.key_name, EXPIRE_KEY_SECONDS)
                            .unwrap();
                    }
                    BackendAction::Get => {
                        let pipe = received.pipeline.unwrap();
                        let results: Vec<Option<f64>> = pipe.query(&mut connection).unwrap();

                        let values = results.into_iter().map(|val| val.unwrap_or(0f64)).collect();

                        received
                            .result_tx
                            .unwrap()
                            .send(RedisJobResult { values })
                            .unwrap();
                    } // BackendAction::Get => {
                      //     let get_result: Result<f64, redis::RedisError> = match received.labels_hash
                      //     {
                      //         Some(labels_hash) => connection.hget(&received.key_name, &labels_hash),
                      //         None => connection.get(&received.key_name),
                      //     };
                      //     let value: f64 = match get_result {
                      //         Ok(value) => {
                      //             // TODO: most likely will need to queue these operations
                      //             // waiting on the expire call before returning the value is not
                      //             // good
                      //             let _: () = connection
                      //                 .expire(&received.key_name, EXPIRE_KEY_SECONDS)
                      //                 .unwrap();
                      //             value
                      //         }
                      //         Err(e) => {
                      //             if e.kind() == redis::ErrorKind::TypeError {
                      //                 // This would happen when there is no key so `nil` is returned
                      //                 // so we return the default 0.0 value
                      //                 0.0
                      //             } else {
                      //                 // TODO: will need to handle the panic
                      //                 panic!("{e:?}");
                      //             }
                      //         }
                      //     };

                      //     received
                      //         .result_tx
                      //         .unwrap()
                      //         .send(RedisJobResult { value })
                      //         .unwrap();
                      // }
                }
            }
        });

        Ok(())
    }

    #[classmethod]
    fn _generate_samples(cls: &PyType, registry: &PyAny) -> PyResult<PyObject> {
        let py = cls.py();
        let collectors = registry.call_method0(intern!(py, "collect"))?;

        let metric_collectors: PyResult<Vec<&PyAny>> = collectors
            .iter()?
            .map(|i| i.and_then(PyAny::extract))
            .collect();

        let mut samples_result_dict = SamplesResultDict::new();

        let mut pipe = redis::pipe();

        // TODO: need to support custom collectors
        for metric_collector in metric_collectors? {
            let mut samples_list: Vec<OutSample> = vec![];

            let samples: PyResult<Vec<&PyAny>> = metric_collector
                .call_method0(intern!(py, "collect"))?
                .iter()?
                .map(|i| i.and_then(PyAny::extract))
                .collect();

            for sample in samples? {
                let sample: Sample = sample.extract()?;

                // struct used for converting from python back into python are different
                // probably because they share the same name with the existing `Sample` class
                let out_sample = OutSample::new(sample.suffix, sample.labels, 0.0);
                samples_list.push(out_sample);

                // pipe the get command
                let key_name = &sample.value.key_name;
                let label_hash = &sample.value.labels_hash;

                match label_hash {
                    Some(label_hash) => pipe.hget(key_name, label_hash),
                    None => pipe.get(key_name),
                };
                pipe.expire(key_name, EXPIRE_KEY_SECONDS).ignore();
            }

            samples_result_dict.collectors.push(metric_collector.into());
            samples_result_dict.samples_vec.push(samples_list);
        }

        let send_tx = {
            let redis_job_tx_mutex = REDIS_JOB_TX.get().unwrap();
            let redis_job_tx = redis_job_tx_mutex.lock().unwrap();
            redis_job_tx.clone()
        };

        let (tx, rx) = mpsc::channel();

        send_tx
            .send(RedisJob {
                action: BackendAction::Get,
                key_name: "".to_string(),
                labels_hash: None,
                value: f64::NAN,
                result_tx: Some(tx),
                pipeline: Some(pipe),
            })
            .unwrap();

        // TODO: release gil

        let samples_result_dict = py.allow_threads(move || {
            let job_result = rx.recv().unwrap();

            // map back the values from redis into the appropriate Sample
            let mut samples_vec_united = vec![];
            for samples_vec in &mut samples_result_dict.samples_vec {
                samples_vec_united.extend(samples_vec);
            }

            for (sample, value) in samples_vec_united.iter_mut().zip(job_result.values) {
                sample.value = value
            }

            samples_result_dict
        });

        samples_result_dict.into_py(py)
    }

    fn inc(&self, value: f64) {
        self.redis_job_tx
            .send(RedisJob {
                action: BackendAction::Inc,
                key_name: self.key_name.clone(),
                labels_hash: self.labels_hash.clone(), // I wonder if only the String inside should be cloned into a new Some
                value,
                result_tx: None,
                pipeline: None,
            })
            .unwrap();
    }

    fn dec(&self, value: f64) {
        self.redis_job_tx
            .send(RedisJob {
                action: BackendAction::Dec,
                key_name: self.key_name.clone(),
                labels_hash: self.labels_hash.clone(),
                value: -value,
                result_tx: None,
                pipeline: None,
            })
            .unwrap();
    }

    fn set(&self, value: f64) {
        self.redis_job_tx
            .send(RedisJob {
                action: BackendAction::Set,
                key_name: self.key_name.clone(),
                labels_hash: self.labels_hash.clone(),
                value,
                result_tx: None,
                pipeline: None,
            })
            .unwrap();
    }

    // fn get(&self) -> f64 {
    //     let (tx, rx) = mpsc::channel();
    //     self.redis_job_tx
    //         .send(RedisJob {
    //             action: BackendAction::Get,
    //             key_name: self.key_name.clone(),
    //             labels_hash: self.labels_hash.clone(),
    //             value: f64::NAN,
    //             result_tx: Some(tx),
    //         })
    //         .unwrap();

    //     // TODO: should free the GIL in here
    //     let job_result = rx.recv().unwrap();
    //     job_result.value
    // }
    // }

    // fn get(&self) -> f64 {
    fn get(self_: PyRef<Self>) -> PyRef<'_, RedisBackend> {
        // This returns 0.0 so that we have a list of Samples ready that we can update the value
        // after retrieving the value from redis. We need this behaviour due to the current mixed
        // architecture.
        // TODO: consider if it makes sense to support the get operation out of the metrics
        // retrieval.
        // 0.0
        self_
    }
}

#[pyclass]
struct SingleProcessBackend {
    #[pyo3(get)]
    config: Py<PyDict>,
    #[pyo3(get)]
    metric: Py<PyAny>,
    #[pyo3(get)]
    histogram_bucket: Option<String>,
    value: Mutex<f64>,
}

#[pymethods]
impl SingleProcessBackend {
    #[new]
    fn new(config: &PyDict, metric: &PyAny, histogram_bucket: Option<String>) -> Self {
        Self {
            config: config.into(),
            metric: metric.into(),
            histogram_bucket,
            value: Mutex::new(0.0),
        }
    }

    fn inc(&mut self, value: f64) {
        let mut data = self.value.lock().unwrap();
        *data += value;
    }

    fn dec(&mut self, value: f64) {
        let mut data = self.value.lock().unwrap();
        *data -= value;
    }

    fn set(&mut self, value: f64) {
        let mut data = self.value.lock().unwrap();
        *data = value;
    }

    fn get(&self) -> f64 {
        let data = self.value.lock().unwrap();
        *data
    }
}

/// A Python module implemented in Rust.
#[pymodule]
fn pytheus_backend_rs(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<RedisBackend>()?;
    m.add_class::<SingleProcessBackend>()?;
    m.add_class::<OutSample>()?;
    Ok(())
}
