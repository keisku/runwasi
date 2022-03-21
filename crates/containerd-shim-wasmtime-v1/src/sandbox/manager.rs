use super::error::Error;
use super::instance::{Instance, Wasi as WasiInstance};
use super::oci;
use super::sandbox;
use crate::services::sandbox_ttrpc::{Manager, ManagerClient};
use anyhow::Context;
use containerd_shim::{
    self as shim, api,
    error::Error as ShimError,
    protos::shim::shim_ttrpc::{create_task, Task},
    protos::ttrpc::{Client, Server},
    publisher::RemotePublisher,
    TtrpcContext, TtrpcResult,
};
use nix::sched::{setns, unshare, CloneFlags};
use oci_spec::runtime;
use std::collections::HashMap;
use std::env::current_dir;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::Arc;
use std::sync::RwLock;
use std::thread;
use ttrpc::context;
use wasmtime::Engine;

pub trait Sandbox: Task + Send + Sync {
    type Instance: Instance;

    fn new(namespace: String, id: String, engine: Engine, publisher: RemotePublisher) -> Self;
}

pub struct Service<T: Sandbox> {
    sandboxes: RwLock<HashMap<String, String>>,
    engine: Engine,
    phantom: std::marker::PhantomData<T>,
}

impl<T: Sandbox> Service<T> {
    pub fn new(engine: Engine) -> Self {
        Service::<T> {
            sandboxes: RwLock::new(HashMap::new()),
            engine: engine,
            phantom: std::marker::PhantomData,
        }
    }
}

impl<T> Manager for Service<T>
where
    T: Sandbox<Instance = WasiInstance> + 'static,
{
    fn create(
        &self,
        _ctx: &TtrpcContext,
        req: sandbox::CreateRequest,
    ) -> TtrpcResult<sandbox::CreateResponse> {
        let mut sandboxes = self.sandboxes.write().unwrap();

        if sandboxes.contains_key(&req.id) {
            return Err(Error::AlreadyExists(req.get_id().to_string()))?;
        }

        let sock = format!("unix://{}/shim.sock", &req.working_directory);

        let publisher = RemotePublisher::new(req.ttrpc_address)?;

        let sb = T::new(
            req.namespace.clone(),
            req.id.clone(),
            self.engine.clone(),
            publisher,
        );
        let task_service = create_task(Arc::new(Box::new(sb)));
        let mut server = Server::new().bind(&sock)?.register_service(task_service);

        sandboxes.insert(req.id.clone(), sock.clone());

        let cfg = oci::spec_from_file(
            Path::new(&req.working_directory)
                .join("config.json")
                .to_str()
                .unwrap(),
        )
        .map_err(|err| Error::InvalidArgument(format!("could not load runtime spec: {}", err)))?;

        let (tx, rx) = std::sync::mpsc::channel::<Result<(), Error>>();

        let id = &req.id;

        match thread::Builder::new()
            .name(format!("{}-sandbox-create", id))
            .spawn(move || {
                let r = start_sandbox(cfg, &mut server);
                tx.send(r).context("could not send sandbox result").unwrap();
            }) {
            Ok(_) => {}
            Err(e) => {
                return Err(Error::Others(format!(
                    "failed to spawn sandbox thread: {}",
                    e
                )))?;
            }
        }

        rx.recv()
            .context("could not receive sandbox result")
            .map_err(|err| Error::Others(format!("{}", err)))??;
        return Ok(sandbox::CreateResponse {
            socket_path: sock,
            ..Default::default()
        });
    }
}

// Note that this changes the current thread's state.
// You probably want to run this in a new thread.
fn start_sandbox(cfg: runtime::Spec, server: &mut Server) -> Result<(), Error> {
    let namespaces = cfg.linux().as_ref().unwrap().namespaces().as_ref().unwrap();
    for ns in namespaces {
        if ns.typ() == runtime::LinuxNamespaceType::Network {
            if ns.path().is_some() {
                let p = ns.path().clone().unwrap();
                let f = File::open(p).context("could not open network namespace")?;
                setns(f.as_raw_fd(), CloneFlags::CLONE_NEWNET)
                    .context("error setting network namespace")?;
                break;
            }

            unshare(CloneFlags::CLONE_NEWNET).context("error unsharing network namespace")?;
        }
    }

    server.start_listen().context("could not start listener")?;
    Ok(())
}

pub struct Shim {}

impl Task for Shim {}

// TODO:: This is a temporary implementation.
// The plan is to move off this Shim type since it is not designed to do what we want.
impl shim::Shim for Shim {
    type T = Self;

    fn new(_runtime_id: &str, _id: &str, _namespace: &str, _config: &mut shim::Config) -> Self {
        return Shim {};
    }

    fn start_shim(&mut self, opts: containerd_shim::StartOpts) -> shim::Result<String> {
        let dir = current_dir().map_err(|err| ShimError::Other(err.to_string()))?;
        let spec = oci::load(dir.join("config.json").to_str().unwrap()).map_err(|err| {
            shim::Error::InvalidArgument(format!("error loading runtime spec: {}", err))
        })?;

        let default = HashMap::new() as HashMap<String, String>;
        let annotations = spec.annotations().as_ref().unwrap_or(&default);

        let sandbox = annotations
            .get("io.kubernetes.cri.sandbox-id")
            .unwrap_or(&opts.id)
            .to_string();

        let client = Client::connect("unix:///run/io.containerd.wasmtime.v1/manager.sock")?;
        let mc = ManagerClient::new(client);

        let addr = match mc.create(
            context::Context::default(),
            &sandbox::CreateRequest {
                id: sandbox.clone(),
                working_directory: dir.as_path().to_str().unwrap().to_string(),
                ttrpc_address: opts.ttrpc_address.clone(),
                ..Default::default()
            },
        ) {
            Ok(res) => res.get_socket_path().to_string(),
            Err(_) => {
                let res = mc.connect(
                    context::Context::default(),
                    &sandbox::ConnectRequest {
                        id: sandbox.clone(),
                        ttrpc_address: opts.ttrpc_address.clone(),
                        ..Default::default()
                    },
                )?;
                res.get_socket_path().to_string()
            }
        };

        return Ok(addr);
    }

    fn wait(&mut self) {
        todo!()
    }

    fn create_task_service(&self, _publisher: RemotePublisher) -> Self::T {
        todo!()
    }

    fn delete_shim(&mut self) -> shim::Result<api::DeleteResponse> {
        todo!()
    }
}