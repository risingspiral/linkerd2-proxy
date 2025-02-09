use super::*;
use crate::{tcp, test_util::*};
use linkerd_app_core::{
    io, profiles,
    svc::{NewService, Service, ServiceExt},
    transport::addrs::OrigDstAddr,
    AddrMatch, IpNet,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::time;

/// Tests that the discover stack propagates errors to the caller.
#[tokio::test(flavor = "current_thread")]
async fn errors_propagate() {
    let _trace = linkerd_tracing::test::trace_init();
    time::pause(); // Run the test with a mocked clock.

    let addr = SocketAddr::new([192, 0, 2, 22].into(), 2220);

    // Mock an inner stack with a service that fails, tracking the number of services built &
    // held.
    let new_count = Arc::new(AtomicUsize::new(0));
    let (handle, stack) = {
        let new_count = new_count.clone();
        support::track::new_service(move |_| {
            new_count.fetch_add(1, Ordering::SeqCst);
            svc::mk(move |_: io::DuplexStream| {
                future::err::<(), Error>(io::Error::from(io::ErrorKind::ConnectionRefused).into())
            })
        })
    };

    let profiles = support::profile::resolver().profile(addr, profiles::Profile::default());

    // Create a profile stack that uses the tracked inner stack.
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(stack)
        .push_discover(profiles)
        .into_inner();

    assert_eq!(
        new_count.load(Ordering::SeqCst),
        0,
        "no services have been created yet"
    );
    assert_eq!(
        handle.tracked_services(),
        0,
        "no services have been created yet"
    );

    // Instantiate a service from the stack so that it instantiates the tracked inner service.
    //
    // The discover stack's buffer does not drive profile resolution (or the inner service)
    // until the service is called?! So we drive this all on a background ask that gets canceled
    // to drop the service reference.
    let svc = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
    let task = spawn_conn(svc);
    // We have to let some time pass for the buffer to drive the profile to readiness.
    time::advance(time::Duration::from_millis(100)).await;
    assert_eq!(
        new_count.load(Ordering::SeqCst),
        1,
        "exactly one service has been created"
    );

    task.await.unwrap().expect_err("service must fail");
}

/// Tests that the discover stack caches resolutions for each unique destination address.
///
/// This test obtains a service, drops it obtains the service again, and then
/// drops it again, testing that only one profile lookup is performed. It also
/// tests that a lookup is performed after a resolution idles out.
#[tokio::test(flavor = "current_thread")]
async fn caches_profiles_until_idle() {
    let _trace = linkerd_tracing::test::trace_init();
    time::pause(); // Run the test with a mocked clock.

    let addr = SocketAddr::new([192, 0, 2, 22].into(), 5550);
    let idle_timeout = time::Duration::from_secs(1);
    let sleep_time = idle_timeout + time::Duration::from_millis(1);

    // Mock an inner stack with a service that never returns, tracking the number of services
    // built & held.
    let stack = |_: _| svc::mk(move |_: io::DuplexStream| future::pending::<Result<(), Error>>());

    let profile_lookups = Arc::new(AtomicUsize::new(0));
    let profiles = {
        let profile = support::profile::resolver().profile(addr, profiles::Profile::default());
        let lookups = profile_lookups.clone();
        svc::mk(move |a: profiles::LookupAddr| {
            lookups.fetch_add(1, Ordering::SeqCst);
            profile.clone().oneshot(a)
        })
    };

    // Create a profile stack that uses the tracked inner stack, configured to drop cached
    // service after `idle_timeout`.
    let cfg = {
        let mut cfg = default_config();
        cfg.discovery_idle_timeout = idle_timeout;
        cfg
    };
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(cfg, rt)
        .with_stack(stack)
        .push_discover(profiles)
        .into_inner();

    assert_eq!(
        profile_lookups.load(Ordering::SeqCst),
        0,
        "no services have been created yet"
    );

    // Instantiate a service from the stack so that it instantiates the tracked inner service.
    //
    // The discover stack's buffer does not drive profile resolution (or the inner service)
    // until the service is called?! So we drive this all on a background ask that gets canceled
    // to drop the service reference.
    let svc0 = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
    let task0 = spawn_conn(svc0);
    // We have to let some time pass for the buffer to drive the profile to readiness.
    time::advance(time::Duration::from_millis(100)).await;
    assert_eq!(
        profile_lookups.load(Ordering::SeqCst),
        1,
        "exactly one profile lookup"
    );

    // Abort the pending task (simulating a disconnect from a client) and obtain the cached
    // service from the stack.
    task0.abort();
    let svc1 = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
    let task1 = spawn_conn(svc1);
    // Let some time pass and ensure the service hasn't been dropped from the stack (because the
    // task is still running).
    time::sleep(sleep_time).await;
    assert_eq!(
        profile_lookups.load(Ordering::SeqCst),
        1,
        "exactly one profile lookup"
    );

    // Cancel the task and ensure the cached service is dropped after the idle timeout expires.
    task1.abort();
    time::sleep(sleep_time).await;

    // When another stack is built for the same target, we create a new service (because the
    // prior service has been idled out).
    let svc2 = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
    let task2 = spawn_conn(svc2);
    // We have to let some time pass for the buffer to drive the profile to readiness.
    time::advance(time::Duration::from_millis(100)).await;
    assert_eq!(
        profile_lookups.load(Ordering::SeqCst),
        2,
        "second profile lookup after idle timeout"
    );

    task2.abort();
}

/// Tests that the discover stack avoids resolutions when the stack is not configured to permit
/// resolutions.
#[tokio::test(flavor = "current_thread")]
async fn no_profiles_when_outside_search_nets() {
    let _trace = linkerd_tracing::test::trace_init();

    let addr = SocketAddr::new([192, 0, 2, 22].into(), 2222);

    // XXX we should assert that the resolver isn't even invoked, but the mocked resolver
    // doesn't support that right now. So, instead, we return a profile for resolutions to
    // and assert (below) that no profile is provided.
    let profiles = support::profile::resolver().profile(addr, profiles::Profile::default());

    // Mock an inner stack with a service that asserts that no profile is built.
    let stack = |d: Discovery<_>| {
        assert!(d.profile.is_none(), "profile must not resolve");
        svc::mk(move |_: io::DuplexStream| future::ok::<(), Error>(()))
    };

    // Create a profile stack that uses the tracked inner stack, configured to never actually do
    // profile resolutions for the IP being tested.

    let cfg = {
        let mut cfg = default_config();
        // Permits resolutions for only 192.0.2.66/32.
        cfg.allow_discovery =
            AddrMatch::new(None, Some(IpNet::from(IpAddr::from([192, 0, 2, 66]))));
        cfg
    };
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(cfg, rt)
        .with_stack(stack)
        .push_discover(profiles)
        .into_inner();

    // Instantiate a service from the stack so that it instantiates the tracked inner service.
    //
    // The discover stack's buffer does not drive profile resolution (or the inner service)
    // until the service is called?! So we drive this all on a background ask that gets canceled
    // to drop the service reference.
    let svc = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
    spawn_conn(svc).await.unwrap().expect("must not fail");
}

fn spawn_conn<S>(mut svc: S) -> tokio::task::JoinHandle<Result<(), Error>>
where
    S: Service<io::DuplexStream, Response = (), Error = Error> + Send + 'static,
    S::Future: Send,
{
    tokio::spawn(async move {
        let (server_io, _client_io) = io::duplex(1);
        svc.ready().await?.call(server_io).await?;
        drop(svc);
        Ok(())
    })
}
