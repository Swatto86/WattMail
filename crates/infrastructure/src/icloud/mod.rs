//! iCloud calendar backend: CalDAV over HTTPS, authenticated with an Apple ID
//! and an app-specific password (Apple offers no OAuth for CalDAV).
//!
//! Kept wholly separate from [`crate::graph`] — the two providers share nothing
//! but the domain contracts they implement.

pub mod calendar;
pub mod civil;
pub mod dav;
pub mod ical;
pub mod rrule;
