// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use core::ops::Range;

pub(crate) const GICD_SIZE: usize = 0x1_0000;

pub(crate) const GICD_CTLR: usize = 0x0000;
pub(crate) const GICD_TYPER: usize = 0x0004;
pub(crate) const GICD_IIDR: usize = 0x0008;
pub(crate) const GICD_TYPER2: usize = 0x000c;
pub(crate) const GICD_STATUSR: usize = 0x0010;

pub(crate) const GICD_IGROUPR: Range<usize> = 0x0080..0x0100;
pub(crate) const GICD_ISENABLER: Range<usize> = 0x0100..0x0180;
pub(crate) const GICD_ICENABLER: Range<usize> = 0x0180..0x0200;
pub(crate) const GICD_ISPENDR: Range<usize> = 0x0200..0x0280;
pub(crate) const GICD_ICPENDR: Range<usize> = 0x0280..0x0300;
pub(crate) const GICD_ISACTIVER: Range<usize> = 0x0300..0x0380;
pub(crate) const GICD_ICACTIVER: Range<usize> = 0x0380..0x0400;
pub(crate) const GICD_IPRIORITYR: Range<usize> = 0x0400..0x0800;
pub(crate) const GICD_ITARGETSR: Range<usize> = 0x0800..0x0c00;
pub(crate) const GICD_ICFGR: Range<usize> = 0x0c00..0x0d00;
pub(crate) const GICD_IROUTER: Range<usize> = 0x6000..0x8000;

pub(crate) const GICD_PIDR2: usize = 0xffe8;
