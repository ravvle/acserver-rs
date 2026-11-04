//! AI traffic (`EnableAi`): the road network from a track's AI splines and
//! the cars that drive it. `server::traffic` connects them to the server.

pub mod spline;
pub mod state;

pub use spline::{AiSpline, BuildOptions};
pub use state::{AiState, AiStatus, CarTuning, JunctionChoices, Obstacle, World};
