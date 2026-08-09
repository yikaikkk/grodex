use grodex_core::id::StepGeneration;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FenceError {
    LateEvent { event_given: StepGeneration, current_expected: StepGeneration },
    MissingGeneration,
    NonMonotonic { attempted: StepGeneration, current: StepGeneration },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationFence {
    current_generation: StepGeneration,
}

impl GenerationFence {
    pub fn new(start: StepGeneration) -> Self { Self { current_generation: start } }
    pub fn current(&self) -> StepGeneration { self.current_generation }

    pub fn accept(
        &self,
        event_generation: Option<StepGeneration>,
        strict: bool,
    ) -> Result<(), FenceError> {
        match event_generation {
            None if strict => Err(FenceError::MissingGeneration),
            None => Ok(()),
            Some(g) if g < self.current_generation => Err(FenceError::LateEvent {
                event_given: g,
                current_expected: self.current_generation,
            }),
            Some(_) => Ok(()),
        }
    }

    pub fn bump(&mut self, new: StepGeneration) -> Result<(), FenceError> {
        if new < self.current_generation {
            return Err(FenceError::NonMonotonic {
                attempted: new,
                current: self.current_generation,
            });
        }
        self.current_generation = new;
        Ok(())
    }
}
