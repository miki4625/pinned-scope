# pinned-scope
****

pinned-scope provides an analogue mechanism of `std::thread::scope` but for async - unfortunately, 1:1 mapping is not possible in today's Rust. It's fully composable on nightly and partially on stable.

## Caveats
- Sharing data with other tasks is **only** available for owned data by the scope
- Not polling to completeness started future provided by scope can result in undesired behavior.  
This could be the result of using the `futures::select!` macro or using `Box::pin`, polling once, and then dropping pinned box.  
Actual behavior:
    - On **stable**: destructor will block the parent thread (and executor itself) until all **started** child tasks are completed.
    - On **nightly** with the `unstable-async-drop` feature turned on: AsyncDrop will yield `Poll::Pending` until all started child tasks are completed.
    - Dropping not completed future returned by the scope will not drive it to completeness, therefore no more child tasks will be spawned.

It's discouraged to use this crate without `AsyncDrop`.

## Example

#### Replication of [std::thread::scope](https://doc.rust-lang.org/std/thread/fn.scope.html) example:
```rust
// We cant share data not owned by the scope (but ofc we can move it in there)
//let mut a = vec![1, 2, 3];
//let mut x = 0;

let (mut a, x) = pinned_scope::tokio_spawner::scope(async |s| { //async closure instead of normal one
  let a = vec![1, 2, 3];
  let mut x = 0;

  let (s, _) = s.spawn(async { // normal async blocks that can capture scope data
    println!("hello from the first scoped task");
    // We can borrow `a` here.
    dbg!(&a);
  }).await; // everytime task is spawned current scope guard is consumed

  let (s, _) = s.spawn(async {
    println!("hello from the second scoped task");
    // We can even mutably borrow `x` here,
    // because no other task is using it.
    x += a[0] + a[2];
  }).await;

  println!("hello from the main thread");
  s.finish((a, x)) // Closure must return guard: Scope<'_, Finish<_>, _>
}).await;

// After the scope, we can modify and access returned variables:
a.push(4);
assert_eq!(x, a.len());
```

But in this example we `await` on spawned task, therefore the first and second spawns will always be invoked sequentially. To fix this we can change the second `spawn` to `with`:
#### Fixed first example
```rust
let (mut a, x) = pinned_scope::tokio_spawner::scope(async |s| { //async closure instead of normal one
  let a = vec![1, 2, 3];
  let mut x = 0;

  let s = s.spawn(async {
    println!("hello from the first scoped task");
    dbg!(&a);
  }); // lack of await therefore task is not started

  let ((s, _), _) = s.with(async {
    println!("hello from the second scoped task");
    x += a[0] + a[2];
  }).await; // await starts both tasks in unordered manner (uses futures::future::join)

  println!("hello from the main thread");
  s.finish((a, x))
}).await;
```

#### How to manage limitations
```rust
async fn get_data() -> String {
  "requested data".to_string()
}

async fn process_data(data: &str) {
  println!("Data: \"{}\" processed", data);
}

async fn save_data(data: &str) {
  println!("Data: \"{}\" saved", data);
}

let data = get_data().await;

/*
// Not possible because scope cannot borrow data from outside
async fn manage_data(data: &str) {
  pinned_scope::tokio_spawner::scope(async move |s| {
    let s = s.spawn(process_data(data));
    let ((s, _), _) = s.with(save_data(data)).await;
    s.finish(())
  }).await;
}

manage_data(&data).await;
*/

// But it's okey to move data into and out of it
async fn manage_data(data: String) -> String {
  pinned_scope::tokio_spawner::scope(async move |s| {
    // Or even get data directly inside
    // let data = get_data().await;
    let s = s.spawn(process_data(&data));
    let ((s, _), _) = s.with(save_data(&data)).await;
    s.finish(data)
  }).await
}

let data = manage_data(data).await;
// And still own data to do something else with it
```



## Why this should be safe and sound

Scopes in async are problematic because it's hard to manage them in safe Rust for two reasons:
1. Outside vicious code  
    1.1 Drop of partially completed main future  
    1.2 Leaking started main future
2. Inside vicious code  
    2.1 Drop of partially completed child futures  
    2.2 Leaking started child futures

#### 1.1 e.g.
```rust
let returned_future = scope(...);
let pinned = pin!(returned_future);
poll!(pinned);
drop(pinned);
```
Solution: Drop will block the parent thread until all started child tasks are completed.  
*On nighly it's possible to use `AsyncDrop` to not block the executor.

#### 1.2 e.g.
```rust
let borrowed_vec = vec![1, 2, 3];
let returned_future = scope(...);
let pinned = Box::pin(returned_future);
poll!(pinned);
std::mem::forget(pinned);
drop(borrowed_vec); // UB: spawned task can still have access to it
```
Solution: Make scope `'static` and `!Unpin`: 
- `'static` - shareable data must be owned by scope.
- `!Unpin` - by `Pin` [drop guarantee](https://doc.rust-lang.org/std/pin/index.html#subtle-details-and-the-drop-guarantee) we know that the memory of the scope will not be invalidated until Drop is called - it's safe to leak the returned future even if it was partially completed, because data borrowed by children tasks will always be valid.

#### 2.1 e.g.
```rust
scope(async |s| {
    let children_future = s.spawn(...);
    let pinned = pin!(returned_future);
    poll!(pinned);
    drop(pinned);
});
```
Solution: Closure must return Scope with Finish guard which, cannot be acquired if not all children tasks are not driven to completion - will not compile.


#### 2.2 e.g.
```rust
scope(async |s| {
    let borrowed_vec = vec![1, 2, 3];
    let children_future = s.spawn(...);
    let pinned = Box::pin(returned_future);
    poll!(pinned);
    std::mem::forget(pinned);
    drop(borrowed_vec); // UB: spawned task can still have access to it
});
```
Solution: Closure must return Scope with Finish guard which, cannot be acquired if not all children tasks are not driven to completion - will not compile.