#ifndef RUST_SCHED_LAUNCHER_H
#define RUST_SCHED_LAUNCHER_H

#include "rust_internal.h"
#include "sync/rust_thread.h"
#include "rust_sched_driver.h"

class rust_sched_launcher : public kernel_owned<rust_sched_launcher> {
public:
    rust_kernel *kernel;

private:
    rust_sched_loop sched_loop;

protected:
    rust_sched_driver driver;

public:
    rust_sched_launcher(rust_scheduler *sched, rust_srv *srv, int id);
    virtual ~rust_sched_launcher() { }

    virtual void start() = 0;
    virtual void join() = 0;
    rust_sched_loop *get_loop() { return &sched_loop; }
};

class rust_thread_sched_launcher
  :public rust_sched_launcher,
   private rust_thread {
public:
    rust_thread_sched_launcher(rust_scheduler *sched, rust_srv *srv, int id);
    virtual void start() { rust_thread::start(); }
    virtual void join() { rust_thread::join(); }
    virtual void run() { driver.start_main_loop(); }
};

class rust_manual_sched_launcher : public rust_sched_launcher {
public:
  rust_manual_sched_launcher(rust_scheduler *sched, rust_srv *srv, int id);
  virtual void start() { }
  virtual void join() { }
  void start_main_loop() { driver.start_main_loop(); }
};

class rust_sched_launcher_factory {
public:
    virtual ~rust_sched_launcher_factory() { }
    virtual rust_sched_launcher *
    create(rust_scheduler *sched, int id) = 0;
};

class rust_thread_sched_launcher_factory
    : public rust_sched_launcher_factory {
public:
    virtual rust_sched_launcher *create(rust_scheduler *sched, int id);
};

#endif // RUST_SCHED_LAUNCHER_H
