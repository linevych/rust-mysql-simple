use std::{
    collections::VecDeque,
    fmt,
    ops::Deref,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration as StdDuration,
};

use crate::{
    prelude::*,
    time::{Duration, SteadyTime},
    Conn, DriverError, Error, IsolationLevel, LocalInfileHandler, Opts, Params, QueryResult,
    Result as MyResult, Statement, Transaction,
};

#[derive(Debug)]
struct InnerPool {
    opts: Opts,
    pool: VecDeque<Conn>,
}

impl InnerPool {
    fn new(min: usize, max: usize, opts: Opts) -> MyResult<InnerPool> {
        if min > max || max == 0 {
            return Err(Error::DriverError(DriverError::InvalidPoolConstraints));
        }
        let mut pool = InnerPool {
            opts,
            pool: VecDeque::with_capacity(max),
        };
        for _ in 0..min {
            pool.new_conn()?;
        }
        Ok(pool)
    }
    fn new_conn(&mut self) -> MyResult<()> {
        match Conn::new(self.opts.clone()) {
            Ok(conn) => {
                self.pool.push_back(conn);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }
}

/// `Pool` serves to provide you with a [`PooledConn`](struct.PooledConn.html)'s.
/// However you can prepare statements directly on `Pool` without
/// invoking [`Pool::get_conn`](struct.Pool.html#method.get_conn).
///
/// `Pool` will hold at least `min` connections and will create as many as `max`
/// connections with possible overhead of one connection per alive thread.
///
/// Example of multithreaded `Pool` usage:
///
/// ```rust
/// # mysql::doctest_wrapper!(__result, {
/// # use mysql::*;
/// # use mysql::prelude::*;
/// # let mut conn = Conn::new(get_opts())?;
/// let opts = get_opts();
/// let pool = Pool::new(opts).unwrap();
/// let mut threads = Vec::new();
///
/// for _ in 0..100 {
///     let pool = pool.clone();
///     threads.push(std::thread::spawn(move || {
///         let mut conn = pool.get_conn().unwrap();
///         let result: u8 = conn.query_first("SELECT 1").unwrap().unwrap();
///         assert_eq!(result, 1_u8);
///     }));
/// }
///
/// for t in threads.into_iter() {
///     assert!(t.join().is_ok());
/// }
/// # });
/// ```
///
/// For more info on how to work with mysql connection please look at
/// [`PooledConn`](struct.PooledConn.html) documentation.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<(Mutex<InnerPool>, Condvar)>,
    min: Arc<AtomicUsize>,
    max: Arc<AtomicUsize>,
    count: Arc<AtomicUsize>,
    check_health: bool,
    use_cache: bool,
}

impl Pool {
    /// Will return connection taken from a pool.
    ///
    /// Will verify and fix it via `Conn::ping` and `Conn::reset` if `call_ping` is `true`.
    /// Will try to get concrete connection if `id` is `Some(_)`.
    /// Will wait til timeout if `timeout_ms` is `Some(_)`
    fn _get_conn<T: AsRef<str>>(
        &self,
        stmt: Option<T>,
        timeout_ms: Option<u32>,
        call_ping: bool,
    ) -> MyResult<PooledConn> {
        let times = if let Some(timeout_ms) = timeout_ms {
            Some((
                SteadyTime::now(),
                Duration::milliseconds(timeout_ms.into()),
                StdDuration::from_millis(timeout_ms.into()),
            ))
        } else {
            None
        };

        let &(ref inner_pool, ref condvar) = &*self.inner;

        let conn = if self.use_cache {
            if let Some(query) = stmt {
                let mut id = None;
                let mut pool = inner_pool.lock()?;
                for (i, conn) in pool.pool.iter().rev().enumerate() {
                    if conn.has_stmt(query.as_ref()) {
                        id = Some(i);
                        break;
                    }
                }
                id.and_then(|id| pool.pool.swap_remove_back(id))
            } else {
                None
            }
        } else {
            None
        };

        let mut conn = if let Some(conn) = conn {
            conn
        } else {
            let out_conn;
            let mut pool = inner_pool.lock()?;
            loop {
                if let Some(conn) = pool.pool.pop_front() {
                    drop(pool);
                    out_conn = Some(conn);
                    break;
                } else if self.count.load(Ordering::Relaxed) < self.max.load(Ordering::Relaxed) {
                    pool.new_conn()?;
                    self.count.fetch_add(1, Ordering::SeqCst);
                } else {
                    pool = if let Some((start, timeout, std_timeout)) = times {
                        if SteadyTime::now() - start > timeout {
                            return Err(DriverError::Timeout.into());
                        }
                        condvar.wait_timeout(pool, std_timeout)?.0
                    } else {
                        condvar.wait(pool)?
                    }
                }
            }
            out_conn.unwrap()
        };

        if call_ping && self.check_health && !conn.ping() {
            conn.reset()?;
        }

        Ok(PooledConn {
            pool: self.clone(),
            conn: Some(conn),
        })
    }

    /// Creates new pool with `min = 10` and `max = 100`.
    pub fn new<T: Into<Opts>>(opts: T) -> MyResult<Pool> {
        Pool::new_manual(10, 100, opts)
    }

    /// Same as `new` but you can set `min` and `max`.
    pub fn new_manual<T: Into<Opts>>(min: usize, max: usize, opts: T) -> MyResult<Pool> {
        let pool = InnerPool::new(min, max, opts.into())?;
        Ok(Pool {
            inner: Arc::new((Mutex::new(pool), Condvar::new())),
            min: Arc::new(AtomicUsize::new(min)),
            max: Arc::new(AtomicUsize::new(max)),
            count: Arc::new(AtomicUsize::new(min)),
            use_cache: true,
            check_health: true,
        })
    }

    /// A way to turn off searching for cached statement (on by default).
    ///
    /// If turned on, then calls to `Pool::{prepare, prep_exec, first_exec}` will search for cached
    /// statement through all connections in the pool. Useless if the value of the `stmt_cache_size`
    /// option is 0.
    pub fn use_cache(&mut self, use_cache: bool) {
        self.use_cache = use_cache;
    }

    /// A way to turn off connection health check on each call to `get_conn` and `prepare`
    /// (`prep_exec` is not affected) (on by default).
    pub fn check_health(&mut self, check_health: bool) {
        self.check_health = check_health;
    }

    /// Gives you a [`PooledConn`](struct.PooledConn.html).
    ///
    /// `Pool` will check that connection is alive via
    /// [`Conn::ping`](struct.Conn.html#method.ping) and will
    /// call [`Conn::reset`](struct.Conn.html#method.reset) if
    /// necessary.
    pub fn get_conn(&self) -> MyResult<PooledConn> {
        self._get_conn(None::<String>, None, true)
    }

    /// Will try to get connection for a duration of `timeout_ms` milliseconds.
    ///
    /// # Failure
    /// This function will return `Error::DriverError(DriverError::Timeout)` if timeout was
    /// reached while waiting for new connection to become available.
    pub fn try_get_conn(&self, timeout_ms: u32) -> MyResult<PooledConn> {
        self._get_conn(None::<String>, Some(timeout_ms), true)
    }

    /// Shortcut for `pool.get_conn()?.start_transaction(..)`.
    pub fn start_transaction(
        &self,
        consistent_snapshot: bool,
        isolation_level: Option<IsolationLevel>,
        readonly: Option<bool>,
    ) -> MyResult<Transaction<'static>> {
        let conn = self._get_conn(None::<String>, None, false)?;
        let result = conn.pooled_start_transaction(consistent_snapshot, isolation_level, readonly);
        match result {
            Ok(trans) => Ok(trans),
            Err(ref e) if e.is_connectivity_error() => {
                let conn = self._get_conn(None::<String>, None, true)?;
                conn.pooled_start_transaction(consistent_snapshot, isolation_level, readonly)
            }
            Err(e) => Err(e),
        }
    }
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Pool {{ min: {}, max: {}, count: {} }}",
            self.min.load(Ordering::Relaxed),
            self.max.load(Ordering::Relaxed),
            self.count.load(Ordering::Relaxed)
        )
    }
}

/// Pooled mysql connection which will return to the pool on `drop`.
///
/// You should prefer using `prepare` or `prep_exec` instead of `query` where possible, except
/// cases when statement has no params and when it has no return values or return values which
/// evaluates to `Value::Bytes`.
///
/// `query` is a part of mysql text protocol, so under the hood you will always receive
/// `Value::Bytes` as a result and `from_value` will need to parse it if you want, for example, `i64`
///
/// ```rust
/// # mysql::doctest_wrapper!(__result, {
/// # use mysql::*;
/// # use mysql::prelude::*;
/// # let mut conn = Conn::new(get_opts())?;
/// let pool = Pool::new(get_opts()).unwrap();
/// let mut conn = pool.get_conn().unwrap();
///
/// conn.query_first("SELECT 42").map(|result: Option<Value>| {
///     let result = result.unwrap();
///     assert_eq!(result, Value::Bytes(b"42".to_vec()));
///     assert_eq!(from_value::<i64>(result), 42i64);
/// }).unwrap();
/// conn.exec_iter("SELECT 42", ()).map(|mut result| {
///     let cell = result.next().unwrap().unwrap().take(0).unwrap();
///     assert_eq!(cell, Value::Int(42i64));
///     assert_eq!(from_value::<i64>(cell), 42i64);
/// }).unwrap();
/// # });
/// ```
///
/// For more info on how to work with query results please look at
/// [`QueryResult`](../struct.QueryResult.html) documentation.
#[derive(Debug)]
pub struct PooledConn {
    pool: Pool,
    conn: Option<Conn>,
}

impl Deref for PooledConn {
    type Target = Conn;

    fn deref(&self) -> &Self::Target {
        self.conn.as_ref().expect("deref after drop")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if self.pool.count.load(Ordering::Relaxed) > self.pool.max.load(Ordering::Relaxed)
            || self.conn.is_none()
        {
            self.pool.count.fetch_sub(1, Ordering::SeqCst);
        } else {
            self.conn.as_mut().unwrap().set_local_infile_handler(None);
            let mut pool = (self.pool.inner).0.lock().unwrap();
            pool.pool.push_back(self.conn.take().unwrap());
            drop(pool);
            (self.pool.inner).1.notify_one();
        }
    }
}

impl PooledConn {
    /// Redirects to
    /// [`Conn#start_transaction`](struct.Conn.html#method.start_transaction)
    pub fn start_transaction(
        &mut self,
        consistent_snapshot: bool,
        isolation_level: Option<IsolationLevel>,
        readonly: Option<bool>,
    ) -> MyResult<Transaction> {
        self.conn.as_mut().unwrap().start_transaction(
            consistent_snapshot,
            isolation_level,
            readonly,
        )
    }

    /// Gives mutable reference to the wrapped
    /// [`Conn`](struct.Conn.html).
    pub fn as_mut(&mut self) -> &mut Conn {
        self.conn.as_mut().unwrap()
    }

    /// Gives reference to the wrapped
    /// [`Conn`](struct.Conn.html).
    pub fn as_ref(&self) -> &Conn {
        self.conn.as_ref().unwrap()
    }

    /// Unwraps wrapped [`Conn`](struct.Conn.html).
    pub fn unwrap(mut self) -> Conn {
        self.conn.take().unwrap()
    }

    fn pooled_start_transaction<'a>(
        mut self,
        consistent_snapshot: bool,
        isolation_level: Option<IsolationLevel>,
        readonly: Option<bool>,
    ) -> MyResult<Transaction<'a>> {
        self.as_mut()
            ._start_transaction(consistent_snapshot, isolation_level, readonly)?;
        Ok(Transaction::new_pooled(self))
    }

    /// A way to override default local infile handler for this pooled connection. Destructor will
    /// restore original handler before returning connection to a pool.
    /// See [`Conn::set_local_infile_handler`](struct.Conn.html#method.set_local_infile_handler).
    pub fn set_local_infile_handler(&mut self, handler: Option<LocalInfileHandler>) {
        self.conn
            .as_mut()
            .unwrap()
            .set_local_infile_handler(handler);
    }
}

impl Queryable for PooledConn {
    fn query_iter<T: AsRef<str>>(&mut self, query: T) -> MyResult<QueryResult<'_>> {
        self.conn.as_mut().unwrap().query_iter(query)
    }

    fn prep<T: AsRef<str>>(&mut self, query: T) -> MyResult<Statement> {
        self.conn.as_mut().unwrap().prep(query)
    }

    fn close(&mut self, stmt: Statement) -> Result<(), Error> {
        self.conn.as_mut().unwrap().close(stmt)
    }

    fn exec_iter<S, P>(&mut self, stmt: S, params: P) -> MyResult<QueryResult<'_>>
    where
        S: AsStatement,
        P: Into<Params>,
    {
        self.conn.as_mut().unwrap().exec_iter(stmt, params)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod test {
    mod pool {
        use std::{thread, time::Duration};

        use crate::{
            from_value, prelude::*, test_misc::get_opts, DriverError, Error, OptsBuilder, Pool,
        };

        #[test]
        fn multiple_pools_should_work() {
            let pool = Pool::new(get_opts()).unwrap();
            pool.get_conn()
                .unwrap()
                .exec_drop("DROP DATABASE IF EXISTS A", ())
                .unwrap();
            pool.get_conn()
                .unwrap()
                .exec_drop("CREATE DATABASE A", ())
                .unwrap();
            pool.get_conn()
                .unwrap()
                .exec_drop("DROP TABLE IF EXISTS A.a", ())
                .unwrap();
            pool.get_conn()
                .unwrap()
                .exec_drop("CREATE TABLE IF NOT EXISTS A.a (id INT)", ())
                .unwrap();
            pool.get_conn()
                .unwrap()
                .exec_drop("INSERT INTO A.a VALUES (1)", ())
                .unwrap();
            let opts = OptsBuilder::from_opts(get_opts()).db_name(Some("A"));
            let pool2 = Pool::new(opts).unwrap();
            let count: u8 = pool2
                .get_conn()
                .unwrap()
                .exec_first("SELECT COUNT(*) FROM a", ())
                .unwrap()
                .unwrap();
            assert_eq!(1, count);
            pool.get_conn()
                .unwrap()
                .exec_drop("DROP DATABASE A", ())
                .unwrap();
        }

        struct A {
            pool: Pool,
            x: u32,
        }

        impl A {
            fn add(&mut self) {
                self.x += 1;
            }
        }

        #[test]
        fn should_fix_connectivity_errors_on_prepare() {
            let pool = Pool::new_manual(2, 2, get_opts()).unwrap();
            let mut conn = pool.get_conn().unwrap();

            let id: u32 = pool
                .get_conn()
                .unwrap()
                .exec_first("SELECT CONNECTION_ID();", ())
                .unwrap()
                .unwrap();

            conn.exec_drop("KILL CONNECTION ?", (id,)).unwrap();
            thread::sleep(Duration::from_millis(250));
            pool.get_conn()
                .unwrap()
                .prep("SHOW FULL PROCESSLIST")
                .unwrap();
        }

        #[test]
        fn should_fix_connectivity_errors_on_prep_exec() {
            let pool = Pool::new_manual(2, 2, get_opts()).unwrap();
            let mut conn = pool.get_conn().unwrap();

            let id: u32 = pool
                .get_conn()
                .unwrap()
                .exec_first("SELECT CONNECTION_ID();", ())
                .unwrap()
                .unwrap();

            conn.exec_drop("KILL CONNECTION ?", (id,)).unwrap();
            thread::sleep(Duration::from_millis(250));
            pool.get_conn()
                .unwrap()
                .exec_drop("SHOW FULL PROCESSLIST", ())
                .unwrap();
        }
        #[test]
        fn should_fix_connectivity_errors_on_start_transaction() {
            let pool = Pool::new_manual(2, 2, get_opts()).unwrap();
            let mut conn = pool.get_conn().unwrap();

            let id: u32 = pool
                .get_conn()
                .unwrap()
                .exec_first("SELECT CONNECTION_ID();", ())
                .unwrap()
                .unwrap();

            conn.exec_drop("KILL CONNECTION ?", (id,)).unwrap();
            thread::sleep(Duration::from_millis(250));
            pool.start_transaction(false, None, None).unwrap();
        }
        #[test]
        fn should_execute_queryes_on_PooledConn() {
            let pool = Pool::new(get_opts()).unwrap();
            let mut threads = Vec::new();
            for _ in 0usize..10 {
                let pool = pool.clone();
                threads.push(thread::spawn(move || {
                    let conn = pool.get_conn();
                    assert!(conn.is_ok());
                    let mut conn = conn.unwrap();
                    conn.query_drop("SELECT 1").unwrap();
                }));
            }
            for t in threads.into_iter() {
                assert!(t.join().is_ok());
            }
        }
        #[test]
        fn should_timeout_if_no_connections_available() {
            let pool = Pool::new_manual(0, 1, get_opts()).unwrap();
            let conn1 = pool.try_get_conn(357).unwrap();
            let conn2 = pool.try_get_conn(357);
            assert!(conn2.is_err());
            match conn2 {
                Err(Error::DriverError(DriverError::Timeout)) => assert!(true),
                _ => assert!(false),
            }
            drop(conn1);
            assert!(pool.try_get_conn(357).is_ok());
        }

        #[test]
        fn should_execute_statements_on_PooledConn() {
            let pool = Pool::new(get_opts()).unwrap();
            let mut threads = Vec::new();
            for _ in 0usize..10 {
                let pool = pool.clone();
                threads.push(thread::spawn(move || {
                    let mut conn = pool.get_conn().unwrap();
                    let stmt = conn.prep("SELECT 1").unwrap();
                    conn.exec_drop(&stmt, ()).unwrap();
                }));
            }
            for t in threads.into_iter() {
                assert!(t.join().is_ok());
            }

            let pool = Pool::new(get_opts()).unwrap();
            let mut threads = Vec::new();
            for _ in 0usize..10 {
                let pool = pool.clone();
                threads.push(thread::spawn(move || {
                    let mut conn = pool.get_conn().unwrap();
                    conn.exec_drop("SELECT ?", (1,)).unwrap();
                }));
            }
            for t in threads.into_iter() {
                assert!(t.join().is_ok());
            }
        }

        #[test]
        #[allow(unused_variables)]
        fn should_start_transaction_on_Pool() {
            let pool = Pool::new_manual(1, 10, get_opts()).unwrap();
            pool.get_conn()
                .unwrap()
                .query_drop("CREATE TEMPORARY TABLE mysql.tbl(a INT)")
                .unwrap();
            pool.start_transaction(false, None, None)
                .and_then(|mut t| {
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(1)").unwrap();
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(2)").unwrap();
                    t.commit()
                })
                .unwrap();
            assert_eq!(
                pool.get_conn()
                    .unwrap()
                    .query_first::<u8, _>("SELECT COUNT(a) FROM mysql.tbl")
                    .unwrap()
                    .unwrap(),
                2_u8
            );
            pool.start_transaction(false, None, None)
                .and_then(|mut t| {
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(1)").unwrap();
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(2)").unwrap();
                    t.rollback()
                })
                .unwrap();
            assert_eq!(
                pool.get_conn()
                    .unwrap()
                    .query_first::<u8, _>("SELECT COUNT(a) FROM mysql.tbl")
                    .unwrap()
                    .unwrap(),
                2_u8
            );
            pool.start_transaction(false, None, None)
                .and_then(|mut t| {
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(1)").unwrap();
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(2)").unwrap();
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                pool.get_conn()
                    .unwrap()
                    .query_first::<u8, _>("SELECT COUNT(a) FROM mysql.tbl")
                    .unwrap()
                    .unwrap(),
                2_u8
            );
            let mut a = A { pool, x: 0 };
            let transaction = a.pool.start_transaction(false, None, None).unwrap();
            a.add();
        }

        #[test]
        fn should_start_transaction_on_PooledConn() {
            let pool = Pool::new(get_opts()).unwrap();
            let mut conn = pool.get_conn().unwrap();
            conn.query_drop("CREATE TEMPORARY TABLE mysql.tbl(a INT)")
                .unwrap();
            conn.start_transaction(false, None, None)
                .and_then(|mut t| {
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(1)").unwrap();
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(2)").unwrap();
                    t.commit()
                })
                .unwrap();
            for x in conn.query_iter("SELECT COUNT(a) FROM mysql.tbl").unwrap() {
                let mut x = x.unwrap();
                assert_eq!(from_value::<u8>(x.take(0).unwrap()), 2u8);
            }
            conn.start_transaction(false, None, None)
                .and_then(|mut t| {
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(1)").unwrap();
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(2)").unwrap();
                    t.rollback()
                })
                .unwrap();
            for x in conn.query_iter("SELECT COUNT(a) FROM mysql.tbl").unwrap() {
                let mut x = x.unwrap();
                assert_eq!(from_value::<u8>(x.take(0).unwrap()), 2u8);
            }
            conn.start_transaction(false, None, None)
                .and_then(|mut t| {
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(1)").unwrap();
                    t.query_drop("INSERT INTO mysql.tbl(a) VALUES(2)").unwrap();
                    Ok(())
                })
                .unwrap();
            for x in conn.query_iter("SELECT COUNT(a) FROM mysql.tbl").unwrap() {
                let mut x = x.unwrap();
                assert_eq!(from_value::<u8>(x.take(0).unwrap()), 2u8);
            }
        }

        #[cfg(feature = "nightly")]
        mod bench {
            use test;

            use std::thread;

            use crate::{test_misc::get_opts, Pool};

            #[bench]
            fn many_prepares(bencher: &mut test::Bencher) {
                let pool = Pool::new(get_opts()).unwrap();
                bencher.iter(|| {
                    pool.prepare("SELECT 1").unwrap();
                });
            }

            #[bench]
            fn many_prepexecs(bencher: &mut test::Bencher) {
                let pool = Pool::new(get_opts()).unwrap();
                bencher.iter(|| {
                    pool.prep_exec("SELECT 1", ()).unwrap();
                });
            }

            #[bench]
            fn many_prepares_threaded(bencher: &mut test::Bencher) {
                let pool = Pool::new(get_opts()).unwrap();
                bencher.iter(|| {
                    let mut threads = Vec::new();
                    for _ in 0..4 {
                        let pool = pool.clone();
                        threads.push(thread::spawn(move || {
                            for _ in 0..250 {
                                test::black_box(
                                    pool.prep_exec(
                                        "SELECT 1, 'hello world', 123.321, ?, ?, ?",
                                        ("hello", "world", 65536),
                                    )
                                    .unwrap(),
                                );
                            }
                        }));
                    }
                    for t in threads {
                        t.join().unwrap();
                    }
                });
            }

            #[bench]
            fn many_prepares_threaded_no_cache(bencher: &mut test::Bencher) {
                let mut pool = Pool::new(get_opts()).unwrap();
                pool.use_cache(false);
                bencher.iter(|| {
                    let mut threads = Vec::new();
                    for _ in 0..4 {
                        let pool = pool.clone();
                        threads.push(thread::spawn(move || {
                            for _ in 0..250 {
                                test::black_box(
                                    pool.prep_exec(
                                        "SELECT 1, 'hello world', 123.321, ?, ?, ?",
                                        ("hello", "world", 65536),
                                    )
                                    .unwrap(),
                                );
                            }
                        }));
                    }
                    for t in threads {
                        t.join().unwrap();
                    }
                });
            }
        }
    }
}
