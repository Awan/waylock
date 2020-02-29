#[macro_use(environment)]
extern crate smithay_client_toolkit as sctk;

use byteorder::{NativeEndian, WriteBytesExt};
use pam::Authenticator;
use sctk::environment::{Environment, SimpleGlobal};
use sctk::output::OutputHandler;
use sctk::reexports::calloop;
use sctk::reexports::client::protocol::{wl_compositor, wl_output, wl_seat, wl_shm, wl_surface};
use sctk::reexports::client::{Attached, DispatchData, Display, Proxy};
use sctk::reexports::protocols::wlr::unstable::{
    input_inhibitor::v1::client::zwlr_input_inhibit_manager_v1,
    layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1},
};
use sctk::seat::{keyboard, keyboard::keysyms, SeatData, SeatHandler, SeatHandling, SeatListener};
use sctk::shm::{MemPool, ShmHandler};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::rc::Rc;
use users::get_current_username;

struct LockEnv {
    compositor: SimpleGlobal<wl_compositor::WlCompositor>,
    layer_shell: SimpleGlobal<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    inhibitor_manager: SimpleGlobal<zwlr_input_inhibit_manager_v1::ZwlrInputInhibitManagerV1>,
    shm: ShmHandler,
    outputs: OutputHandler,
    seats: SeatHandler,
}

impl SeatHandling for LockEnv {
    fn listen<F: FnMut(Attached<wl_seat::WlSeat>, &SeatData, DispatchData) + 'static>(
        &mut self,
        f: F,
    ) -> SeatListener {
        self.seats.listen(f)
    }
}

environment!(LockEnv,
    singles = [
        wl_compositor::WlCompositor => compositor,
        zwlr_layer_shell_v1::ZwlrLayerShellV1 => layer_shell,
        zwlr_input_inhibit_manager_v1::ZwlrInputInhibitManagerV1 => inhibitor_manager,
        wl_shm::WlShm => shm,
    ],
    multis = [
        wl_output::WlOutput => outputs,
        wl_seat::WlSeat => seats,
    ]
);

#[derive(PartialEq, Copy, Clone)]
enum RenderEvent {
    Configure { width: u32, height: u32 },
    Frame,
    Close,
}

fn main() {
    let (lock_env, display, queue) = {
        let display = Display::connect_to_env().unwrap();
        let mut queue = display.create_event_queue();
        let lock_env = Environment::init(
            &Proxy::clone(&display).attach(queue.token()),
            LockEnv {
                compositor: SimpleGlobal::new(),
                layer_shell: SimpleGlobal::new(),
                inhibitor_manager: SimpleGlobal::new(),
                shm: ShmHandler::new(),
                outputs: OutputHandler::new(),
                seats: SeatHandler::new(),
            },
        );
        let ret = queue.sync_roundtrip(&mut (), |_, _, _| unreachable!());
        ret.and_then(|_| queue.sync_roundtrip(&mut (), |_, _, _| unreachable!()))
            .expect("Error during initial setup");

        (lock_env, display, queue)
    };

    let _inhibitor = lock_env
        .require_global::<zwlr_input_inhibit_manager_v1::ZwlrInputInhibitManagerV1>()
        .get_inhibitor();

    let next_render_event = Rc::new(Cell::new(None::<RenderEvent>));

    let mut pools = lock_env
        .create_double_pool(|_| {})
        .expect("Failed to create a memory pool !");

    // TODO: support multiple outputs
    // TODO: set opaque region
    let surface = lock_env.create_surface();
    // This configuration is copied from swaylock
    let layer_surface = lock_env
        .require_global::<zwlr_layer_shell_v1::ZwlrLayerShellV1>()
        .get_layer_surface(
            &surface,
            Some(&lock_env.get_all_outputs().first().unwrap()),
            zwlr_layer_shell_v1::Layer::Overlay,
            "lockscreen".to_owned(),
        );
    layer_surface.set_size(0, 0);
    layer_surface.set_anchor(zwlr_layer_surface_v1::Anchor::all());
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(1);
    // Commit so that the server will send a configure event
    surface.commit();

    let next_render_event_handle = Rc::clone(&next_render_event);
    layer_surface.quick_assign(move |layer_surface, event, _| {
        match (event, next_render_event_handle.get()) {
            (zwlr_layer_surface_v1::Event::Closed, _) => {
                next_render_event_handle.set(Some(RenderEvent::Close));
            }
            (
                zwlr_layer_surface_v1::Event::Configure {
                    serial,
                    width,
                    height,
                },
                next,
            ) if next != Some(RenderEvent::Close) => {
                layer_surface.ack_configure(serial);
                next_render_event_handle.set(Some(RenderEvent::Configure { width, height }));
            }
            (_, _) => {}
        }
    });

    let mut seats = HashMap::new();
    let input_queue = Rc::new(RefCell::new(VecDeque::new()));
    let mut event_loop = calloop::EventLoop::<()>::new().unwrap();

    // first process already existing seats
    for seat in lock_env.get_all_seats() {
        if let Some((has_kbd, name)) = sctk::seat::with_seat_data(&seat, |seat_data| {
            (
                seat_data.has_keyboard && !seat_data.defunct,
                seat_data.name.clone(),
            )
        }) {
            if has_kbd {
                let input_queue_handle = input_queue.clone();
                match keyboard::map_keyboard(
                    &seat,
                    None,
                    keyboard::RepeatKind::System,
                    move |event, _, _| handle_keyboard_event(event, Rc::clone(&input_queue_handle)),
                ) {
                    Ok((kbd, repeat_source)) => {
                        // Need to put the repeat_source in our event loop or key repitition won't
                        // work
                        let source = event_loop
                            .handle()
                            .insert_source(repeat_source, |_, _| {})
                            .unwrap();
                        seats.insert(name, Some((kbd, source)));
                    }
                    Err(e) => {
                        eprintln!("Failed to map keyboard on seat {} : {:?}.", name, e);
                        seats.insert(name, None);
                    }
                }
            } else {
                seats.insert(name, None);
            }
        }
    }

    // then setup a listener for changes
    let input_queue_handle = Rc::clone(&input_queue);
    let loop_handle = event_loop.handle();
    let _seat_listener = lock_env.listen_for_seats(move |seat, seat_data, _| {
        seats
            .entry(seat_data.name.clone())
            .and_modify(|opt_kbd| {
                // map a keyboard if the seat has the capability and is not defunct
                if seat_data.has_keyboard && !seat_data.defunct {
                    if opt_kbd.is_none() {
                        // initalize the keyboard
                        let input_queue_handle_handle = Rc::clone(&input_queue_handle);
                        match keyboard::map_keyboard(
                            &seat,
                            None,
                            keyboard::RepeatKind::System,
                            move |event, _, _| {
                                handle_keyboard_event(event, Rc::clone(&input_queue_handle_handle))
                            },
                        ) {
                            Ok((kbd, repeat_source)) => {
                                let source =
                                    loop_handle.insert_source(repeat_source, |_, _| {}).unwrap();
                                *opt_kbd = Some((kbd, source));
                            }
                            Err(e) => eprintln!(
                                "Failed to map keyboard on seat {} : {:?}.",
                                seat_data.name, e
                            ),
                        }
                    }
                } else if let Some((kbd, source)) = opt_kbd.take() {
                    // the keyboard has been removed, cleanup
                    kbd.release();
                    source.remove();
                }
            })
            .or_insert(None);
    });

    let _source_queue = event_loop
        .handle()
        .insert_source(sctk::WaylandSource::new(queue), |ret, _| {
            if let Err(e) = ret {
                panic!("Wayland connection lost: {:?}", e);
            }
        })
        .unwrap();

    let mut authenticator =
        Authenticator::with_password("system-auth").expect("Failed to init PAM client!");
    let current_username = get_current_username().unwrap().into_string().unwrap();
    let mut current_password = String::new();

    let mut redraw = false;
    let mut dimensions = (0, 0);

    // Solarized base03
    let mut current_color = 0xFF002B36;

    loop {
        match next_render_event.replace(None) {
            Some(RenderEvent::Close) => {
                // TODO: cleanup needed?
                break;
            }
            Some(RenderEvent::Configure { width, height }) => {
                redraw = true;
                dimensions = (width, height);
            }
            Some(RenderEvent::Frame) => {
                redraw = true;
            }
            None => {}
        }

        if redraw {
            if let Some(pool) = pools.pool() {
                draw(pool, &surface, current_color, dimensions);
            }
        }

        while let Some((keysym, utf8)) = input_queue.borrow_mut().pop_front() {
            match keysym {
                keysyms::XKB_KEY_KP_Enter | keysyms::XKB_KEY_Return => {
                    authenticator
                        .get_handler()
                        .set_credentials(&current_username, &current_password);
                    match authenticator.authenticate() {
                        Ok(()) => return,
                        Err(error) => {
                            eprintln!("Authentication failed: {}", error);
                            // Solarized red
                            current_color = 0xFFDC322F;
                            redraw = true;
                        }
                    }
                }
                keysyms::XKB_KEY_Delete | keysyms::XKB_KEY_BackSpace => {
                    current_password.pop();
                }
                keysyms::XKB_KEY_Escape => {
                    current_password.clear();
                }
                _ => {
                    if let Some(new_input) = utf8 {
                        current_password.push_str(&new_input);
                    }
                }
            }
        }

        display.flush().unwrap();
        event_loop.dispatch(None, &mut ()).unwrap();
    }
}

// TODO: better error handling, this should return a Result<>
fn draw(
    pool: &mut MemPool,
    surface: &wl_surface::WlSurface,
    color: u32,
    (width, height): (u32, u32),
) {
    // First make sure the pool is large enough
    pool.resize((4 * width * height) as usize)
        .expect("Failure on shm pool resize");

    // Write the color to all bytes of the pool
    pool.seek(SeekFrom::Start(0)).unwrap();
    {
        let mut writer = BufWriter::new(&mut *pool);
        for _ in 0..(width * height) {
            writer.write_u32::<NativeEndian>(color).unwrap();
        }
        writer.flush().unwrap();
    }

    // Create a new buffer from the pool
    let buffer = pool.buffer(
        0,
        width as i32,
        height as i32,
        4 * width as i32,
        wl_shm::Format::Argb8888,
    );

    // Attach the buffer to the surface and mark the entire surface as damaged
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, width as i32, height as i32);

    // Finally, commit the surface
    surface.commit();
}

fn handle_keyboard_event(
    event: keyboard::Event,
    input_queue_handle: Rc<RefCell<VecDeque<(u32, Option<String>)>>>,
) {
    match event {
        keyboard::Event::Key {
            keysym,
            state: keyboard::KeyState::Pressed,
            utf8,
            ..
        } => {
            input_queue_handle.borrow_mut().push_back((keysym, utf8));
        }
        _ => {}
    }
}
