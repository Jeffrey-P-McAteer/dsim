// Guess who doesn't care right now?
#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]
#![allow(non_camel_case_types)]
#![allow(unreachable_code)]

use std::cell::{RefCell, RefMut};
use std::rc::Rc;
use std::borrow::Borrow;

use clap::Parser;
use num_format::{Locale, ToFormattedString};
use plotters::prelude::*;
use plotters::coord::types::RangedCoordf32;

pub mod structs;
pub mod utils;


fn main() -> Result<(), Box<dyn std::error::Error>>  {
  let args = structs::Args::parse();

  let rt  = tokio::runtime::Builder::new_multi_thread()
    .worker_threads(std::cmp::max(2, num_cpus::get_physical())) // Use all host cores, unless single-cored in which case pretend to have 2
    .thread_stack_size(8 * 1024 * 1024)
    .enable_time()
    .enable_io()
    .build()?;

  rt.block_on(async {
    if let Err(e) = main_async(&args).await {
      eprintln!("[ main_async ] {}", e);
      std::process::exit(1);
    }
  });

  Ok(())
}




async fn main_async(args: &structs::Args) -> Result<(), Box<dyn std::error::Error>> {
  let total_start = std::time::Instant::now();

  let mut simcontrol = utils::read_simcontrol_file(&args.simcontrol_file_path).await.map_err(structs::eloc!())?;
  // Overwrite any simcontrol args w/ cli-specified args
  utils::inplace_update_simcontrol_from_args(&mut simcontrol, args);
  let simcontrol = simcontrol;

  if args.verbose >= 2 {
    println!("simcontrol = {:#?}", simcontrol);
  }


  let pref_dev_id = utils::get_pref_device(&simcontrol.preferred_gpu_name.to_lowercase()).await.map_err(structs::eloc!())?;
  let device = opencl3::device::Device::new(pref_dev_id);
  if let Ok(name) = device.name() {
    if args.verbose >= 1 {
      println!("Selected Compute device: {}", name);
    }
  }

  let t0_data = utils::read_ld_file(&simcontrol.input_data_file_path).await;
  let mut cl_kernels = utils::read_cl_kernel_file(&simcontrol.cl_kernels_file_path).await.map_err(structs::eloc!())?.kernel;

  if args.verbose >= 2 {
    println!("t0_data = {:#?}", &t0_data);
    println!("cl_kernels = {:#?}", &cl_kernels);
  }

  let context = opencl3::context::Context::from_device(&device).map_err(structs::eloc!())?;

  let device_init_end = std::time::Instant::now();
  eprintln!("Hardware Initialization: {}", utils::duration_to_display_str(&(device_init_end - total_start)));

  // Compile cl_kernel source code to programs
  let kernel_compile_start = std::time::Instant::now();
  for i in 0..cl_kernels.len() {
    cl_kernels[i].load_program(&context).map_err(structs::eloc!())?;
  }
  let kernel_compile_end = std::time::Instant::now();
  eprintln!("CL Kernel Compile Time: {}", utils::duration_to_display_str(&(kernel_compile_end - kernel_compile_start)));

  video_rs::init()?;

  let encoder_width_usize = simcontrol.output_animation_width as usize;
  let encoder_height_usize = simcontrol.output_animation_height as usize;
  let settings = video_rs::encode::Settings::preset_h264_yuv420p(encoder_width_usize, encoder_height_usize, false);
  let mut encoder: Option<video_rs::encode::Encoder> = None;
  if !(simcontrol.output_animation_file_path.to_string_lossy() == "/dev/null" || simcontrol.output_animation_file_path.to_string_lossy() == "NUL") {
    encoder = Some(video_rs::encode::Encoder::new(simcontrol.output_animation_file_path.clone(), settings).map_err(structs::eloc!())?);
  }
  let anim_frame_duration = video_rs::time::Time::from_secs_f64(simcontrol.output_animation_frame_delay as f64 / 1000.0f64);
  let mut anim_t_position = video_rs::time::Time::zero();

  let mut plotter_dt = raqote::DrawTarget::new(simcontrol.output_animation_width as i32, simcontrol.output_animation_height as i32);
  let plotter_dt_f32width = simcontrol.output_animation_width as f32;
  let plotter_dt_f32height = simcontrol.output_animation_height as f32;
  let plotter_dt_solid_white = raqote::Source::Solid(raqote::SolidSource::from_unpremultiplied_argb(0xff, 255, 255, 255));
  let plotter_dt_solid_black = raqote::Source::Solid(raqote::SolidSource::from_unpremultiplied_argb(0xff, 0,     0,   0));
  let plotter_dt_default_drawops = raqote::DrawOptions::new();
  let plotter_dt_font_bytes = include_bytes!("Courier_New.ttf");
  let plotter_ft_font_typed = std::sync::Arc::new(plotter_dt_font_bytes.to_vec());
  let plotter_dt_font = <font_kit::loaders::freetype::Font as font_kit::loader::Loader>::from_bytes(
    plotter_ft_font_typed, 0
  )?;

  let mut bgr_px_buff: Vec<u8> = vec![0; encoder_height_usize * encoder_width_usize * 3]; // allocate space for the BGR values
  let mut ndarr_data = ndarray::Array3::from_shape_vec((encoder_height_usize, encoder_width_usize, 3), bgr_px_buff.clone()).map_err(structs::eloc!())?;

  let simulation_start = std::time::Instant::now();

  // Each step we go in between ListedData (sim_data) and a utils::ld_data_to_kernel_data vector; eventually
  // the best approach is to keep everything in a utils::ld_data_to_kernel_data format & map indexes between kernels so they read/write the same data.
  let mut sim_data = t0_data.clone();

  // anim_point_history is used as a circular buffer
  let mut anim_point_history: Vec<(f32, f32)> = vec![(0.0, 0.0); sim_data.len() * simcontrol.max_historic_entity_locations];
  let mut anim_point_history_i = 0;

  // For performance reasons we pre-allocate all entity colors here and re-use
  // when plotting data. This means there will be NO capability to change an entity color in the middle of
  // a sim; and if there were I'd want to provide the API as an "index into known colors" anyhow.
  let mut sim_data_colors: Vec<raqote::Source> = vec![];
  for row in sim_data.iter() {
    if let Some(str_val) = row.get(&simcontrol.gis_color_attr) {
      match csscolorparser::parse(str_val.to_string().as_str()) {
        Ok(css_color_obj) => {
          let components = css_color_obj.to_rgba8();
          //sim_data_colors.push( plotters::style::RGBColor(components[0], components[1], components[2]) );
          sim_data_colors.push( raqote::Source::Solid(raqote::SolidSource::from_unpremultiplied_argb(0xff, components[0], components[1], components[2])) );
        }
        Err(e) => {
          if args.verbose > 0 {
            eprintln!("{:?}", e);
          }
         sim_data_colors.push(plotter_dt_solid_black.clone());
        }
      }
    }
    else {
       sim_data_colors.push(plotter_dt_solid_black.clone());
    }
  }

  //let mut sim_data_rasterized_argb: Vec<> = vec![];


  // We also read an arbitrary background image, or use white as a background for the renderer.
  let sim_bg_argb_frame: Vec<u32> = if simcontrol.background_img.len() > 0 {
    let img = image::ImageReader::open(simcontrol.background_img)?.decode()?;


    std::unimplemented!()
  }
  else {
    vec![0xffffffffu32, ((simcontrol.output_animation_width * simcontrol.output_animation_height) as usize).try_into().unwrap()]
  };


  let mut total_kernel_execs_duration = std::time::Duration::from_millis(0);
  let mut total_convert_overhead_duration = std::time::Duration::from_millis(0);
  let mut total_gis_paint_duration = std::time::Duration::from_millis(0);

  // Allocate long-term CL data
  let queue = opencl3::command_queue::CommandQueue::create_default_with_properties(&context, opencl3::command_queue::CL_QUEUE_PROFILING_ENABLE, 0).expect("CommandQueue::create_default failed");

  // Both vectors must be kept in-sync; we keep sim_events_cl so we can rapidly pass a pointer to always-valid CL event structures
  let mut sim_events: Vec<opencl3::event::Event> = vec![];
  let mut sim_events_cl: Vec<opencl3::types::cl_event> = vec![];

  // For each kernel, convert LD data to Kernel data;
  // For each new (Name,Type) pair add to a all_kernel vector of tagged CL buffers.
  // We then store argument indexes into the all_kernel vector for individual kernels,
  // allowing re-use of the same buffers across the entire simulation.
  let mut all_kernel_args: Vec<structs::CL_NamedTaggedArgument> = vec![];
  let mut all_kernel_arg_indicies: Vec<Vec<usize>> = vec![];
  for i in 0..cl_kernels.len() {
    if let Some(k) = &cl_kernels[i].cl_device_kernel {

      let ld_to_kernel_start = std::time::Instant::now();
      let kernel_args = utils::ld_data_to_kernel_data_named(&args, &simcontrol, &sim_data, &context, &cl_kernels[i], &k, &queue, &sim_events_cl).map_err(structs::eloc!())?;
      let ld_to_kernel_end = std::time::Instant::now();
      total_convert_overhead_duration += ld_to_kernel_end - ld_to_kernel_start;

      let mut this_kernel_ak_indicies: Vec<usize> = vec![];

      for kai in 0..kernel_args.len() {
        let mut all_kernel_args_existing_idx: Option<usize> = None;
        for akai in 0..all_kernel_args.len() {
          if kernel_args[kai].name == all_kernel_args[akai].name && std::mem::discriminant::<structs::CL_TaggedArgument>(kernel_args[kai].tagged_argument.borrow()) == std::mem::discriminant::<structs::CL_TaggedArgument>(all_kernel_args[akai].tagged_argument.borrow()) {
            // Name & Type matches, store index directly
            all_kernel_args_existing_idx = Some(akai);
            break;
          }
        }

        match all_kernel_args_existing_idx {
          Some(akai_idx) => {
            this_kernel_ak_indicies.push(akai_idx);
          }
          None => {
            // New name,type must be added to all_kernel_args.
            // Calling .clone() will make the interior .tagged_argument read-only until kernel_args is dropped at the end of this cl_kernels[i] loop iteration.
            this_kernel_ak_indicies.push(all_kernel_args.len());
            all_kernel_args.push(
              kernel_args[kai].clone()
            );
          }
        }

      }

      all_kernel_arg_indicies.push(this_kernel_ak_indicies);

    }
  }

  // Inspect & Panic if any of the interior .tagged_argument Arcs are not mutable; we require these to be mutable downstairs.
  for akai in 0..all_kernel_args.len() {
    if std::sync::Arc::<structs::CL_TaggedArgument>::get_mut(&mut all_kernel_args[akai].tagged_argument).is_none() {
      eprintln!("Logic error! all_kernel_args[{}].tagged_argument was supposed to be mutable, but is not!", akai);
      panic!("Logic error!");
    }
  }
  if args.verbose > 0 {
    eprintln!("all_kernel_arg_indicies = {:?}", all_kernel_arg_indicies);
  }

  // Finally, we must create & inject "Conversion Kernels" into the stream where we have
  // Variable A of type A followed by Variable A of type B in all_kernel_args.
  // ^^ TODO


  for sim_step_i in 0..simcontrol.num_steps {
    // For each kernel, read in sim_data, process that data, then transform back mutating sim_data itself.
    for i in 0..cl_kernels.len() {
      if let Some(k) = &cl_kernels[i].cl_device_kernel {

        let kernel_exec_start = std::time::Instant::now();

        // Allocate a runtime kernel & feed it inputs; we use RefCell here b/c otherwise inner-loop lifetimes would kill us
        let mut exec_kernel = opencl3::kernel::ExecuteKernel::new(&k);

        for aka_idx in all_kernel_arg_indicies[i].iter() {
          let arg = &all_kernel_args[*aka_idx].clone();
          unsafe {
            match arg.tagged_argument.borrow() {
              structs::CL_TaggedArgument::Uint8Buffer(a)  => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Uint16Buffer(a) => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Uint32Buffer(a) => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Uint64Buffer(a) => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Int8Buffer(a)   => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Int16Buffer(a)  => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Int32Buffer(a)  => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Int64Buffer(a)  => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::FloatBuffer(a)  => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::DoubleBuffer(a) => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Uint8(a)        => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Uint16(a)       => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Uint32(a)       => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Uint64(a)       => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Int8(a)         => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Int16(a)        => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Int32(a)        => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Int64(a)        => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Float(a)        => {let exec_kernel = exec_kernel.set_arg(a);},
              structs::CL_TaggedArgument::Double(a)       => {let exec_kernel = exec_kernel.set_arg(a);},
            }
          }
        }

        { // Set the global work size is the number of entitied being simulated
          let exec_kernel = exec_kernel.set_global_work_size( sim_data.len() );
        }

        // Setup command queue
        let mut kernel_event = unsafe { exec_kernel.enqueue_nd_range(&queue).map_err(structs::eloc!())? };

        // Safety: both vectors increase at same time
        sim_events_cl.push(kernel_event.get());
        sim_events.push(kernel_event);

        let kernel_exec_end = std::time::Instant::now();
        total_kernel_execs_duration += kernel_exec_end - kernel_exec_start;

      }
      else {
        eprintln!("[ Fatal Error ] Kernel {} does not have a cl_device_kernel! Inspect hardware & s/w to ensure kernels compile when loaded.", cl_kernels[i].name);
        return Ok(());
      }
    }

    // Every N or so steps trim the events vector on the assumption some have completed
    if sim_step_i % 20 == 0 {
      utils::trim_completed_events(&args, &mut sim_events, &mut sim_events_cl).map_err(structs::eloc!())?;
    }

    // Finally possibly render a frame of data to gif_plot
    if sim_step_i % simcontrol.capture_step_period == 0 {
      if let Some(ref mut encoder) = encoder {

        let kernel_to_ld_start = std::time::Instant::now();
        utils::kernel_data_update_ld_data_named(&args, &context, &queue, &sim_events_cl, &all_kernel_args, &mut sim_data).map_err(structs::eloc!())?;
        let kernel_to_ld_end = std::time::Instant::now();
        total_convert_overhead_duration += kernel_to_ld_end - kernel_to_ld_start;

        let render_start = std::time::Instant::now();
        // Render!

        let udt_height = plotter_dt.height();
        let udt_width = plotter_dt.width();
        let udt = UnsafeDrawTarget(plotter_dt.get_data_mut().into());

        write_frame_to_dt(&sim_bg_argb_frame, udt_width, udt_height, &udt);

        // Render entity histories as small dots in parallel
        //let mut join_set = tokio::task::JoinSet::new();
        tokio_scoped::scope(|scope| {
          for historic_xy_slice in anim_point_history.chunks(4096) {
            scope.spawn( write_historic_xy_points_to_dt(historic_xy_slice, udt_width, udt_height, &udt) );
          }
        });

        // For each entity, if an gis_x_attr_name and gis_y_attr_name coordinate are known and are numeric,
        // render a dot with a label from gis_name_attr
        for row_i in 0..sim_data.len() {
          if let (Some(x_val), Some(y_val)) = (sim_data[row_i].get(&simcontrol.gis_x_attr_name), sim_data[row_i].get(&simcontrol.gis_y_attr_name)) {
            if let (Ok(x_f32), Ok(y_f32)) = (x_val.to_f32(), y_val.to_f32()) {
              // Render!
              plotter_dt.fill_rect(
                x_f32-1.0f32, y_f32-1.0f32,
                3.0f32, 3.0f32,
                &sim_data_colors[row_i],
                &plotter_dt_default_drawops
              );

              // Write text at same y but x+8px to right
              if row_i < simcontrol.max_entity_idx_to_name {
                let mut label_s = sim_data[row_i].get(&simcontrol.gis_name_attr).map(|v| v.to_string()).unwrap_or_else(|| format!("{}", row_i));
                plotter_dt.draw_text(
                  &plotter_dt_font,
                  15.0,
                  &label_s,
                  raqote::Point::new(x_f32 + 8.0f32, y_f32),
                  &plotter_dt_solid_black,
                  &plotter_dt_default_drawops
                );
              }

              // Safety; anim_point_history_i begins at 0 and we never allow it to be >= .len() below
              unsafe { *(anim_point_history.get_unchecked_mut(anim_point_history_i)) = (x_f32, y_f32); }
              anim_point_history_i += 1;
              if anim_point_history_i >= anim_point_history.len() {
                anim_point_history_i = 0;
              }

            }
          }
        }

        // Draw sim step in lower-left corner
        let sim_step_txt = format!("{:_>9}", sim_step_i);

        plotter_dt.draw_text(
          &plotter_dt_font,
          15.0,
          &sim_step_txt,
          raqote::Point::new(plotter_dt_f32width - 86.0f32, plotter_dt_f32height - 16.0f32),
          &plotter_dt_solid_black,
          &plotter_dt_default_drawops
        );

        // Finally add plotter_dt frame to video stream
        let plotter_frame_pixel_data = plotter_dt.get_data_u8(); // with the order BGRA on little endian

        if let Some(mut ndarr_data) = ndarr_data.as_slice_mut() {
          let mut ndarr_px_i = 0;
          for dt_px_i in (0..plotter_frame_pixel_data.len()).step_by(4) {
            ndarr_data[ndarr_px_i] = plotter_frame_pixel_data[dt_px_i];
            ndarr_data[ndarr_px_i+1] = plotter_frame_pixel_data[dt_px_i+1];
            ndarr_data[ndarr_px_i+2] = plotter_frame_pixel_data[dt_px_i+2];
            ndarr_px_i += 3;
          }
        }

        encoder.encode(&ndarr_data, anim_t_position).map_err(structs::eloc!())?;

        anim_t_position = anim_t_position.aligned_with(anim_frame_duration).add();

        let render_end = std::time::Instant::now();
        total_gis_paint_duration += render_end- render_start;
      }
    }

  }


  if sim_events.len() > 0 {
    loop {
      if args.verbose > 0 {
        eprintln!("Waiting for {} events to complete...", sim_events.len());
      }

      for wait_i in 0..40 {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        utils::trim_completed_events(&args, &mut sim_events, &mut sim_events_cl).map_err(structs::eloc!())?;

        if sim_events.len() < 1 {
          break;
        }
      }
      if sim_events.len() < 1 {
        eprintln!("All sim events complete!");
        break;
      }
    }
  }

  // Finishes writing to disk
  if let Some(ref mut encoder) = encoder {
    encoder.finish().map_err(structs::eloc!())?;
  }


  let simulation_end = std::time::Instant::now();
  eprintln!("Simulation Time: {}", utils::duration_to_display_str(&(simulation_end - simulation_start)));

  eprintln!("Simulation Time Kernel Exec: {}", utils::duration_to_display_str(&total_kernel_execs_duration));
  eprintln!("Simulation Time Convert Overhead: {}", utils::duration_to_display_str(&total_convert_overhead_duration));
  eprintln!("Simulation Time Paint: {}", utils::duration_to_display_str(&total_gis_paint_duration));

  // Write to simcontrol.output_data_file_path
  utils::write_ld_file(args, &sim_data, &simcontrol.output_data_file_path).await.map_err(structs::eloc!())?;

  // Write to simcontrol.output_animation_file_path


  let total_end = std::time::Instant::now();
  eprintln!("Total Time: {}", utils::duration_to_display_str(&(total_end - total_start)));

  if let Some(cmd_txt) = &args.post_sim_cmd {
    tokio::process::Command::new("sh")
      .arg("-c")
      .arg(&cmd_txt)
      .spawn()?
      .wait().await?;
  }

  Ok(())
}

fn write_frame_to_dt(argb_frame: &[u32], draw_buffer_width: i32, draw_buffer_height: i32, draw_buffer: &UnsafeDrawTarget<'_>) {

}

async fn write_historic_xy_points_to_dt(historic_xy_slice: &[(f32, f32)], draw_buffer_width: i32, draw_buffer_height: i32, draw_buffer: &UnsafeDrawTarget<'_>) {
  let draw_buffer: &mut [u32] = unsafe { &mut *draw_buffer.0.get() };
  // dra_buffer has (A << 24) | (R << 16) | (G << 8) | B representation
  for (historic_x, historic_y) in historic_xy_slice {
    let (historic_x, historic_y) = (*historic_x as f32, *historic_y as f32);
    let db_x = historic_x as i32;
    let db_y = historic_y as i32;
    let db_offset = (db_y * draw_buffer_width) + db_x;
    draw_buffer[db_offset as usize] = 0x00;
  }
}

// Type safety goes out the window when I need threads throwing pixels into a buffer
struct UnsafeDrawTarget<'a>(std::cell::UnsafeCell<&'a mut [u32]>);
unsafe impl Send for UnsafeDrawTarget<'_> {}
unsafe impl Sync for UnsafeDrawTarget<'_> {}


