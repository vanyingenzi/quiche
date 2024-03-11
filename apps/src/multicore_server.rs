
use crate::args::*;

pub fn multicore_start_server(
    args: ServerArgs, conn_args: CommonArgs
) {
    info!("Started multicore server {:?} {:?}", args, conn_args);
}