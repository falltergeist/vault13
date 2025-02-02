use enumflags2::BitFlags;
use enumflags2_derive::EnumFlags;
use enum_primitive_derive::Primitive;
use if_chain::if_chain;
use slotmap::{SecondaryMap, SlotMap};
use std::cell::RefCell;
use std::{cmp, fmt};
use std::mem;
use std::rc::Rc;

use crate::asset::{CritterAnim, EntityKind, Flag, FlagExt, ItemKind, WeaponKind};
use crate::asset::frame::{FrameId, FrameDb};
use crate::asset::proto::{self, CritterKillKind, ProtoId, ProtoDb};
use crate::asset::script::ProgramId;
use crate::game::script::{Scripts, Sid};
use crate::graphics::{EPoint, Point, Rect};
use crate::graphics::geometry::TileGridView;
use crate::graphics::geometry::hex::{self, Direction, TileGrid};
use crate::graphics::geometry::hex::path_finder::*;
use crate::graphics::lighting::light_grid::{LightTest, LightTestResult};
use crate::graphics::render::Canvas;
use crate::graphics::sprite::*;
use crate::sequence::cancellable::Cancel;
use crate::util::{EnumExt, SmKey, VecExt};
use crate::util::array2d::Array2d;
use crate::vm::PredefinedProc;

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub enum ObjectProtoId {
    None,
    /// Special ID used for dude object (packed as 0x1000000).
    Dude,
    ProtoId(ProtoId),
}

impl ObjectProtoId {
    pub fn proto_id(self) -> Option<ProtoId> {
        if let ObjectProtoId::ProtoId(v) = self {
            Some(v)
        } else {
            None
        }
    }

    pub fn from_packed(v: u32) -> Option<Self> {
        match v {
            0xffffffff => Some(ObjectProtoId::None),
            0x01000000 => Some(ObjectProtoId::Dude),
            _ => ProtoId::from_packed(v).map(ObjectProtoId::ProtoId),
        }
    }

    pub fn pack(self) -> u32 {
        match self {
            ObjectProtoId::None => 0xffffffff,
            ObjectProtoId::Dude => 0x01000000,
            ObjectProtoId::ProtoId(v) => v.pack(),
        }
    }

    pub fn read(rd: &mut impl std::io::Read) -> std::io::Result<Self> {
        use byteorder::{BE, ReadBytesExt};
        use std::io::{Error, ErrorKind};
        let v = rd.read_u32::<BE>()?;
        Self::from_packed(v)
            .ok_or_else(|| Error::new(ErrorKind::InvalidData,
                format!("malformed object PID: {:x}", v)))
    }
}

impl fmt::Debug for ObjectProtoId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ObjectProtoId::None => write!(f, "ObjectProtoId::Null"),
            ObjectProtoId::Dude => write!(f, "ObjectProtoId::Dude"),
            ObjectProtoId::ProtoId(v) => write!(f, "ObjectProtoId(0x{:08x})", v.pack()),
        }
    }
}

impl From<ProtoId> for ObjectProtoId {
    fn from(v: ProtoId) -> Self {
        ObjectProtoId::ProtoId(v)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outline {
    pub style: OutlineStyle,
    pub translucent: bool,
    pub disabled: bool,
}

#[derive(Clone, Debug)]
pub struct Inventory {
    pub capacity: usize,
    pub items: Vec<InventoryItem>,
}

impl Inventory {
    pub fn new() -> Self {
        Self {
            capacity: 0,
            items: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct InventoryItem {
    pub object: Handle,
    pub count: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LightEmitter {
    pub intensity: u32,
    pub radius: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Egg {
    pub pos: Point,
    pub fid: FrameId,
}

impl Egg {
    #[must_use]
    pub fn hit_test(&self, p: Point, tile_grid: &impl TileGridView, frm_db: &FrameDb) -> bool {
        let screen_pos = tile_grid.to_screen(self.pos) + Point::new(16, 8);
        let frms = frm_db.get(self.fid).unwrap();
        let frml = &frms.frame_lists[Direction::NE];
        let frm = &frml.frames[0];

        let bounds = frm.bounds_centered(screen_pos, frml.center);
        if !bounds.contains(p) {
            return false;
        }
        let p = p - bounds.top_left();
        frm.mask.test(p)
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    /// Hit a translucent object.
    pub translucent: bool,

    /// Hit a `Wall` or `Scenery` object at point which is masked by the Egg.
    pub with_egg: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CantTalkSpatial {
    Unreachable,
    TooFar,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct Handle(SmKey);

impl Handle {
    #[cfg(test)]
    pub fn null() -> Self {
        use slotmap::Key;
        Handle(Key::null())
    }
}

#[derive(Debug)]
pub struct Object {
    pub flags: BitFlags<Flag>,
    pub pos: Option<EPoint>,
    pub screen_pos: Point,
    pub screen_shift: Point,
    pub fid: FrameId,
    pub frame_idx: usize,
    pub direction: Direction,
    pub light_emitter: LightEmitter,
    pub pid: ObjectProtoId,
    pub inventory: Inventory,
    pub outline: Option<Outline>,
    pub sequence: Option<Cancel>,
    pub script: Option<(Sid, ProgramId)>,
    pub sub: SubObject,
}

impl Object {
    pub fn new(fid: FrameId, pid: ObjectProtoId, pos: Option<EPoint>) -> Self {
        Self {
            pos,
            screen_pos: Point::new(0, 0),
            screen_shift: Point::new(0, 0),
            fid,
            frame_idx: 0,
            direction: Direction::NE,
            flags: BitFlags::empty(),
            pid,
            inventory: Inventory::new(),
            light_emitter: LightEmitter {
                intensity: 0,
                radius: 0,
            },
            outline: None,
            sequence: None,
            script: None,
            sub: match fid.kind() {
                EntityKind::Critter => SubObject::Critter(Default::default()),
                _ => SubObject::None,
            }
        }
    }

    pub fn kind(&self) -> EntityKind {
        self.fid.kind()
    }

    pub fn has_running_sequence(&self) -> bool {
        self.sequence.as_ref().map(|seq| seq.is_running()).unwrap_or(false)
    }

    pub fn cancel_sequence(&mut self) {
        if let Some(signal) = self.sequence.take() {
            signal.cancel();
        }
    }

    pub fn render(&mut self, canvas: &mut Canvas, light: u32,
            frm_db: &FrameDb, proto_db: &ProtoDb, tile_grid: &impl TileGridView,
            egg: Option<&Egg>) {
        if self.flags.contains(Flag::TurnedOff) {
            return;
        }

        let light = if self.fid.kind() == EntityKind::Interface {
            0x10000
        } else {
            light
        };

        let effect = self.get_effect(proto_db, tile_grid, egg);
        let sprite = self.create_sprite(light, effect, tile_grid);

        self.screen_pos = sprite.render(canvas, frm_db).top_left();
    }

    pub fn render_outline(&self, canvas: &mut Canvas, frm_db: &FrameDb, tile_grid: &impl TileGridView) {
        if self.flags.contains(Flag::TurnedOff) {
            return;
        }
        if let Some(outline) = self.outline {
            if outline.disabled {
                return;
            }
            let effect = Effect::Outline {
                style: outline.style,
                translucent: outline.translucent,
            };
            let sprite = self.create_sprite(0x10000, Some(effect), tile_grid);
            sprite.render(canvas, frm_db);
        }
    }

    // obj_bound()
    pub fn bounds(&self, frm_db: &FrameDb, tile_grid: &impl TileGridView) -> Rect {
        self.do_with_frame_list(frm_db, |frml, frm|
            self.bounds0(frml.center, frm.size(), tile_grid))
    }

    // critter_is_dead()
    pub fn is_critter_dead(&self) -> bool {
        // FIXME
        false
    }

    // obj_intersects_with
    #[must_use]
    pub fn hit_test(&self, p: Point, frm_db: &FrameDb, tile_grid: &impl TileGridView) -> Option<Hit> {
        if self.flags.contains(Flag::TurnedOff) {
            return None;
        }

        let bounds = self.bounds(frm_db, tile_grid);
        if !bounds.contains(p) {
            return None;
        }

        let p = p - bounds.top_left();
        if !self.do_with_frame(frm_db, |frm| frm.mask.test(p)) {
            return None;
        }

        let translucent = self.has_trans() && !self.flags.contains(Flag::TransNone);
        Some(Hit {
            translucent,
            with_egg: false,
        })
    }

    #[must_use]
    pub fn distance(&self, other: &Object) -> Option<u32> {
        let mut r = hex::distance(self.pos?.point, other.pos?.point);
        if r > 0 && self.flags.contains(Flag::MultiHex) {
            r -= 1;
        }
        if r > 0 && other.flags.contains(Flag::MultiHex) {
            r -= 1;
        }
        Some(r)
    }

    fn bounds0(&self, frame_center: Point, frame_size: Point, tile_grid: &impl TileGridView) -> Rect {
        let mut r = if let Some(pos) = self.pos {
            let top_left =
                tile_grid.to_screen(pos.point)
                + Point::new(16, 8)
                + frame_center
                + self.screen_shift
                - Point::new(frame_size.x / 2, frame_size.y - 1);
            let bottom_right = top_left + frame_size;
            Rect::with_points(top_left, bottom_right)
        } else {
            Rect::with_points(self.screen_pos, self.screen_pos + frame_size)
        };

        let has_outline = self.outline.map(|o| !o.disabled).unwrap_or(false);
        if has_outline {
            // Include 1-pixel outline.
            r.left -= 1;
            r.top -= 1;
            r.right += 1;
            r.bottom += 1;
        }

        r
    }

    fn create_sprite(&self, light: u32, effect: Option<Effect>, tile_grid: &impl TileGridView) -> Sprite {
        let (pos, centered) = if let Some(EPoint { point: hex_pos, .. }) = self.pos {
            (tile_grid.to_screen(hex_pos) + self.screen_shift + Point::new(16, 8), true)
        } else {
            (self.screen_pos, false)
        };
        Sprite {
            pos,
            centered,
            fid: self.fid,
            frame_idx: self.frame_idx,
            direction: self.direction,
            light,
            effect,
        }
    }

    fn get_effect(&self, proto_db: &ProtoDb, tile_grid: &impl TileGridView, egg: Option<&Egg>)
            -> Option<Effect> {
        let kind = self.fid.kind();

        if kind == EntityKind::Interface {
            return None;
        }

        let with_egg =
            egg.is_some()
            // Doesn't have any translucency flags.
            && !self.has_trans()
            // Scenery or wall with position and proto.
            && (kind == EntityKind::Scenery || kind == EntityKind::Wall)
                && self.pos.is_some() && self.pid.proto_id().is_some();

        if !with_egg {
            return self.get_trans_effect();
        }

        let egg = egg.unwrap();

        let pos = self.pos.unwrap().point;
        let proto_flags_ext = proto_db.proto(self.pid.proto_id().unwrap()).unwrap().flags_ext;

        let with_egg = if proto_flags_ext.intersects(
                FlagExt::WallEastOrWest | FlagExt::WallWestCorner) {
            hex::is_in_front_of(pos, egg.pos)
                && (!hex::is_to_right_of(egg.pos, pos)
                    || !self.flags.contains(Flag::WallTransEnd))
        } else if proto_flags_ext.contains(FlagExt::WallNorthCorner) {
            hex::is_in_front_of(pos, egg.pos)
                || hex::is_to_right_of(pos, egg.pos)
        } else if proto_flags_ext.contains(FlagExt::WallSouthCorner) {
            hex::is_in_front_of(pos, egg.pos)
                && hex::is_to_right_of(pos, egg.pos)
        } else if hex::is_to_right_of(pos, egg.pos) {
            !hex::is_in_front_of(egg.pos, pos)
                && !self.flags.contains(Flag::WallTransEnd)
        } else {
            false
        };

        if with_egg {
            let mask_pos = tile_grid.to_screen(egg.pos) + Point::new(16, 8)/*+ self.screen_shift ??? */;
            Some(Effect::Masked { mask_fid: egg.fid, mask_pos })
        } else {
            self.get_trans_effect()
        }
    }

    fn get_trans_effect(&self) -> Option<Effect> {
        match () {
            _ if self.flags.contains(Flag::TransEnergy) => Some(Translucency::Energy),
            _ if self.flags.contains(Flag::TransGlass) => Some(Translucency::Glass),
            _ if self.flags.contains(Flag::TransRed) => Some(Translucency::Red),
            _ if self.flags.contains(Flag::TransSteam) => Some(Translucency::Steam),
            _ if self.flags.contains(Flag::TransWall) => Some(Translucency::Wall),
            _ => None,
        }.map(Effect::Translucency)
    }

    fn do_with_frame_list<F, R>(&self, frm_db: &FrameDb, f: F) -> R
        where F: FnOnce(&FrameList, &Frame) -> R
    {
        let direction = self.direction;
        let frame_idx = self.frame_idx;
        let frms = frm_db.get(self.fid).unwrap();
        let frml = &frms.frame_lists[direction];
        let frm = &frml.frames[frame_idx];
        f(frml, frm)
    }

    fn do_with_frame<F, R>(&self, frm_db: &FrameDb, f: F) -> R
        where F: FnOnce(&Frame) -> R
    {
        self.do_with_frame_list(frm_db, |_, frm| f(frm))
    }

    fn has_trans(&self) -> bool {
        self.flags.intersects(
            Flag::TransEnergy | Flag::TransGlass | Flag::TransRed | Flag::TransSteam |
            Flag::TransWall | Flag::TransNone)
    }
}

pub struct Objects {
    tile_grid: TileGrid,
    proto_db: Rc<ProtoDb>,
    frm_db: Rc<FrameDb>,
    handles: SlotMap<SmKey, ()>,
    objects: SecondaryMap<SmKey, RefCell<Object>>,
    // Objects attached to tile (Object::pos is Some).
    by_pos: Box<[Array2d<Vec<Handle>>]>,
    // Objects not attached to tile (Object::pos is None).
    detached: Vec<Handle>,
    empty_object_handle_vec: Vec<Handle>,
    path_finder: RefCell<PathFinder>,
}

impl Objects {
    pub fn new(tile_grid: TileGrid, elevation_count: u32, proto_db: Rc<ProtoDb>,
            frm_db: Rc<FrameDb>) -> Self {
        let path_finder = RefCell::new(PathFinder::new(tile_grid.clone(), 5000));
        let by_pos = Vec::from_fn(elevation_count as usize,
            |_| Array2d::with_default(tile_grid.width() as usize, tile_grid.height() as usize))
            .into_boxed_slice();
        Self {
            tile_grid,
            proto_db,
            frm_db,
            handles: SlotMap::with_key(),
            objects: SecondaryMap::new(),
            by_pos,
            detached: Vec::new(),
            empty_object_handle_vec: Vec::new(),
            path_finder,
        }
    }

    pub fn elevation_count(&self) -> u32 {
        self.by_pos.len() as u32
    }

    pub fn contains(&self, obj: Handle) -> bool {
        self.objects.contains_key(obj.0)
    }

    pub fn clear(&mut self) {
        self.handles.clear();
        self.objects.clear();
        for elev in self.by_pos.iter_mut() {
            for v in elev.as_slice_mut() {
                *v = Vec::new();
            }
        }
        self.detached.clear();
    }

    pub fn insert(&mut self, obj: Object) -> Handle {
        let pos = obj.pos;

        let k = self.handles.insert(());
        let h = Handle(k);
        self.objects.insert(k, RefCell::new(obj));

        self.insert_into_tile_grid(h, pos, true);

        h
    }

    pub fn at(&self, pos: EPoint) -> &Vec<Handle> {
        self.by_pos[pos.elevation as usize]
            .get(pos.point.x as usize, pos.point.y as usize)
            .unwrap()
    }

    pub fn get(&self, h: Handle) -> &RefCell<Object> {
        &self.objects[h.0]
    }

    pub fn light_test(&self, light_test: LightTest) -> LightTestResult {
        let mut update = true;

        let dir = light_test.direction;

        for &objh in self.at(light_test.point) {
            let obj = self.get(objh).borrow();
            if obj.flags.contains(Flag::TurnedOff) {
                continue;
            }
            let block = !obj.flags.contains(Flag::LightThru);

            if obj.fid.kind() == EntityKind::Wall {
                if !obj.flags.contains(Flag::Flat) {
                    let flags_ext = self.proto_db.proto(obj.pid.proto_id().unwrap()).unwrap().flags_ext;
                    if flags_ext.contains(FlagExt::WallEastOrWest) ||
                            flags_ext.contains(FlagExt::WallEastCorner) {
                        if dir != Direction::W
                                && dir != Direction::NW
                                && (dir != Direction::NE || light_test.i >= 8)
                                && (dir != Direction::SW || light_test.i <= 15) {
                            update = false;
                        }
                    } else if flags_ext.contains(FlagExt::WallNorthCorner) {
                        if dir != Direction::NE && dir != Direction::NW {
                            update = false;
                        }
                    } else if flags_ext.contains(FlagExt::WallSouthCorner) {
                        if dir != Direction::NE
                                && dir != Direction::E
                                && dir != Direction::W
                                && dir != Direction::NW
                                && (dir != Direction::SW || light_test.i <= 15) {
                            update = false;
                        }
                    } else if dir != Direction::NE
                            && dir != Direction::E
                            && (dir != Direction::NW || light_test.i <= 7) {
                        update = false;
                    }
                }
            } else if block && dir >= Direction::E && dir <= Direction::SW {
                update = false;
            }

            if block {
                return LightTestResult {
                    block,
                    update,
                }
            }
        }

        LightTestResult {
            block: false,
            update,
        }
    }

    pub fn render(&self, canvas: &mut Canvas, elevation: u32, screen_rect: Rect,
            tile_grid: &impl TileGridView, egg: Option<&Egg>,
            get_light: impl Fn(Option<EPoint>) -> u32) {
        let ref get_light = get_light;
        self.render0(canvas, elevation, screen_rect, tile_grid, egg, get_light, true);
        self.render0(canvas, elevation, screen_rect, tile_grid, egg, get_light, false);
    }

    pub fn render_outlines(&self, canvas: &mut Canvas, elevation: u32, screen_rect: Rect,
            tile_grid: &impl TileGridView) {
        let hex_rect = Self::get_render_hex_rect(screen_rect, tile_grid);
        for y in hex_rect.top..hex_rect.bottom {
            for x in (hex_rect.left..hex_rect.right).rev() {
                let pos = EPoint {
                    elevation,
                    point: Point::new(x, y),
                };
                for &objh in self.at(pos) {
                    let obj = self.get(objh).borrow_mut();
                    obj.render_outline(canvas, &self.frm_db, tile_grid);
                }
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item=Handle> + '_ {
        // FIXME this should come from by_pos.
        self.handles.keys().map(|k| Handle(k))
    }

    pub fn set_pos(&mut self, h: Handle, pos: EPoint) {
        self.remove_from_tile_grid(h);
        self.insert_into_tile_grid(h, Some(pos), true);
    }

    pub fn set_screen_shift(&mut self, h: Handle, shift: Point) {
        let pos = self.remove_from_tile_grid(h);
        self.get(h).borrow_mut().screen_shift = shift;
        self.insert_into_tile_grid(h, pos, false);
    }

    pub fn add_screen_shift(&mut self, h: Handle, shift: Point) -> Point {
        let pos = self.remove_from_tile_grid(h);
        let new_shift = {
            let mut obj = self.get(h).borrow_mut();
            obj.screen_shift += shift;
            obj.screen_shift
        };
        self.insert_into_tile_grid(h, pos, false);
        new_shift
    }

    pub fn reset_screen_shift(&mut self, h: Handle) {
        let pos = self.remove_from_tile_grid(h);
        self.insert_into_tile_grid(h, pos, true);
    }

    // dude_stand()
    pub fn make_standing(&mut self, h: Handle, frm_db: &FrameDb) {
        let shift = {
            let mut obj = self.get(h).borrow_mut();
            let mut shift = Point::new(0, 0);
            let fid = if let FrameId::Critter(critter_fid) = obj.fid {
                if critter_fid.weapon() != WeaponKind::Unarmed {
                    let fid = critter_fid
                        .with_anim(CritterAnim::TakeOut)
                        .into();
                    let frame_set = frm_db.get(fid).unwrap();
                    for frame in &frame_set.frame_lists[obj.direction].frames {
                        shift += frame.shift;
                    }

                    let fid = critter_fid
                        .with_anim(CritterAnim::Stand)
                        .with_weapon(WeaponKind::Unarmed)
                        .into();
                    shift += frm_db.get(fid).unwrap().frame_lists[obj.direction].center;
                }
                let anim = if critter_fid.anim() == CritterAnim::FireDance {
                    CritterAnim::FireDance
                } else {
                    CritterAnim::Stand
                };
                critter_fid
                    .with_anim(anim)
                    .into()
            } else {
                obj.fid
            };
            obj.fid = fid;
            obj.frame_idx = 0;
            shift
        };
        self.set_screen_shift(h, shift);
    }

    // obj_blocking_at()
    #[must_use]
    pub fn is_blocked_at(&self, obj: Handle, pos: EPoint) -> bool {
        let check = |h| {
            if h == obj {
                return false;
            }
            let o = self.get(h).borrow();
            match o.fid.kind() {
                | EntityKind::Critter
                | EntityKind::Scenery
                | EntityKind::Wall
                => {},
                _ => return false,
            }
            if o.flags.contains(Flag::TurnedOff) || o.flags.contains(Flag::NoBlock) {
                return false;
            }
            true
        };
        for &objh in self.at(pos) {
            if check(objh) {
                return true;
            }
        }
        for dir in Direction::iter() {
            if let Some(near) = self.tile_grid.go(pos.point, dir, 1) {
                for &objh in self.at(near.elevated(pos.elevation)) {
                    if self.get(objh).borrow().flags.contains(Flag::MultiHex) {
                        if check(objh) {
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    /// Returns `true` if there's object that would block sight from `obj` through tile at `pos`.
    // obj_sight_blocking_at()
    #[must_use]
    pub fn is_sight_blocked_at(&self, obj: Handle, pos: EPoint) -> bool {
        for &h in self.at(pos) {
            let o = &self.get(h).borrow();
            if !o.flags.contains(Flag::TurnedOff) &&
                !o.flags.contains(Flag::LightThru) &&
                (o.kind() == EntityKind::Scenery || o.kind() == EntityKind::Wall) &&
                h != obj
            {
                return true;
            }
        }
        false
    }

    // obj_shoot_blocking_at()
    #[must_use]
    pub fn shot_blocker_at(&self, obj: Handle, pos: EPoint) -> Option<Handle> {
        let check = |pos, multi_hex_only| {
            for &h in self.at(pos) {
                let o = &self.get(h).borrow();
                if multi_hex_only && !o.flags.contains(Flag::MultiHex) {
                    return None;
                }
                let non_shoot_thru = if multi_hex_only {
                    // TODO For multi-hex ShootThru flag doesn't apply?
                    false
                } else {
                    !o.flags.contains(Flag::ShootThru)
                };
                if !o.flags.contains(Flag::TurnedOff) &&
                    (!o.flags.contains(Flag::NoBlock) || non_shoot_thru) &&
                    h != obj &&
                    match o.kind() {
                        EntityKind::Scenery | EntityKind::Wall | EntityKind::Critter => true,
                        _ => false,
                    }
                {
                    return Some(h);
                }
            }
            None
        };

        let r = check(pos, false);
        if r.is_some() {
            return r;
        }

        // Check for MultiHex objects in neighbor tiles.
        for dir in Direction::iter() {
            if let Some(p) = self.tile_grid.go(pos.point, dir, 1) {
                let r = check(p.elevated(pos.elevation), true);
                if r.is_some() {
                    return r;
                }
            }
        }

        None
    }

    // combat_is_shot_blocked()
    #[must_use]
    pub fn is_shot_blocked(&self, shooter: Handle, target: Handle) -> bool {
        let pos = self.get(shooter).borrow().pos.unwrap();
        let target_pos = self.get(target).borrow().pos.unwrap();
        assert_eq!(pos.elevation, target_pos.elevation);
        let mut last_blocker = None;
        for p in hex::ray(pos.point, target_pos.point) {
            let blocker = self.shot_blocker_at(shooter, p.elevated(pos.elevation));

            if_chain! {
                if blocker != last_blocker;
                if let Some(blocker) = blocker;
                then {
                    if blocker != shooter && blocker != target {
                        let o = self.get(blocker).borrow();
                        if o.kind() != EntityKind::Critter {
                            return true;
                        }
                    }
                    last_blocker = Some(blocker);
                }
            };
            if p == target_pos.point {
                break;
            }
        }
        false
    }

    /// Based on spatial information are the objects able to talk?
    /// Objects can talk if:
    /// 1. There's a path between them which is not sight-blocked (see `sight_blocker_for_object()`).
    /// 2. Screen distance between objects is within the limit.
    // action_can_talk_to()
    pub fn can_talk(&self, obj1: Handle, obj2: Handle) -> Result<(), CantTalkSpatial> {
        let o1 = self.get(obj1).borrow();
        let o2 = self.get(obj2).borrow();

        // TODO maybe return Unreachable error instead.
        let p1 = o1.pos.unwrap();
        let p2 = o2.pos.unwrap();

        if p1.elevation != p2.elevation {
            return Err(CantTalkSpatial::Unreachable);
        }

        if hex::distance(p1.point, p2.point) > 12 {
            return Err(CantTalkSpatial::TooFar);
        }

        let reachable = self.path_finder.borrow_mut().find(p1.point, p2.point, true,
            |p| {
                let p = EPoint::new(p1.elevation, p);
                if self.is_sight_blocked_at(obj1, p) {
                    TileState::Blocked
                } else {
                    TileState::Passable(0)
                }
            }).is_some();
        if reachable {
            Ok(())
        } else {
            Err(CantTalkSpatial::Unreachable)
        }
    }

    // can_talk_to
    pub fn can_talk_now(&self, obj1: Handle, obj2: Handle) -> bool {
        self.distance(obj1, obj2).unwrap() < 9 && !self.is_shot_blocked(obj1, obj2)
    }

    /// Whether `obj` can be talked to.
    // obj_action_can_talk_to()
    pub fn can_talk_to(&self, obj: Handle) -> bool {
        let obj = self.get(obj).borrow();
        if_chain! {
            if let SubObject::Critter(c) = &obj.sub;
            if let Some(pid) = obj.pid.proto_id();
            then {
                c.is_active() && self.proto_db.can_talk_to(pid)
            } else {
                false
            }
        }
    }

    // obj_action_can_use()
    pub fn can_use(&self, obj: Handle) -> bool {
        if let Some(pid) = self.get(obj).borrow().pid.proto_id() {
            match pid {
                | ProtoId::ACTIVE_DYNAMITE
                | ProtoId::ACTIVE_FLARE
                | ProtoId::ACTIVE_PLASTIC_EXPLOSIVE
                => false,
                _ => self.proto_db.can_use(pid),
            }
        } else {
            false
        }
    }

    // item_get_type()
    pub fn item_kind(&self, obj: Handle) -> Option<ItemKind> {
        let obj = self.get(obj).borrow();
        if obj.kind() == EntityKind::Item {
            let pid = obj.pid.proto_id().unwrap();
            if pid == ProtoId::SHIV {
                return Some(self.proto_db.proto(pid).unwrap()
                    .sub.item().unwrap()
                    .sub.kind());
            }
        }
        None
    }

    // action_can_be_pushed()
    pub fn can_push(&self, pusher: Handle, pushed: Handle, scripts: &Scripts,
        in_combat: bool) -> bool
    {
        let pushedo = self.get(pushed).borrow();
        if pushedo.kind() != EntityKind::Critter
            || pusher == pushed
            || !pushedo.sub.critter().unwrap().is_active()
            || !self.can_talk(pusher, pushed).is_ok()
            || pushedo.script.is_none()
        {
            return false;
        }
        let (sid, _) = pushedo.script.unwrap();
        if !scripts.has_predefined_proc(sid, PredefinedProc::Push) {
            return false;
        }
        if in_combat {
            unimplemented!("TODO")
//            pushed_team_num = pushed->_._.critter.combat_data.team_num;
//          pushed_ = &pushed->_._;
//          if ( pushed_team_num == pusher->_._.critter.combat_data.team_num
//            && pusher == pushed_->critter.combat_data.who_hit_me )
//          {
//            return 0;
//          }
//          v7 = pushed_->critter.combat_data.who_hit_me;
//          if ( v7 && v7->_._.critter.combat_data.team_num == pusher->_._.critter.combat_data.team_num )
//            result = 0;
//          else
//            result = 1;
        }
        true
    }

    /// `allow_neighbor_tile` - allows constructing path to a neighbor tile of `to` tile if the
    /// target tile is blocked.
    #[must_use]
    pub fn path(&self,
        obj: Handle,
        to: Point,
        smooth: bool,
        allow_neighbor_tile: bool,
        proto_db: &ProtoDb)
        -> Option<Vec<Direction>>
    {
        let o = self.get(obj).borrow();
        let from = o.pos?;

        let to_blocked = if allow_neighbor_tile {
            Some(self.is_blocked_at(obj, to.elevated(from.elevation)))
        } else {
            None
        };

        let mut r = self.path_finder.borrow_mut().find(from.point, to, smooth,
            |p| {
                let p = EPoint::new(from.elevation, p);
                if (!allow_neighbor_tile || p.point != to) &&
                    self.is_blocked_at(obj, p) // TODO check anim_can_use_door_(obj, v22)
                {
                    TileState::Blocked
                } else if let Some(pid) = o.pid.proto_id() {
                    let radioactive_goo = self.at(p)
                        .iter()
                        .any(|&h| self.get(h).borrow().pid.proto_id()
                            .map(|pid| pid.is_radioactive_goo())
                            .unwrap_or(false));
                    let cost = if radioactive_goo {
                        let gecko = if let proto::SubProto::Critter(ref c) = proto_db.proto(pid).unwrap().sub {
                            c.kill_kind == CritterKillKind::Gecko
                        } else {
                            false
                        };
                        if gecko {
                            100
                        } else {
                            400
                        }
                    } else {
                        0
                    };

                    TileState::Passable(cost)
                } else {
                    TileState::Passable(0)
                }
            });
        if to_blocked == Some(true) {
            if let Some(path) = r.as_mut() {
                let last = path.len() - 1;
                path.remove(last);
            }
        }
        r
    }

    pub fn bounds(&self, obj: Handle, tile_grid: &impl TileGridView) -> Rect {
        self.get(obj).borrow().bounds(&self.frm_db, tile_grid)
    }

    pub fn hit_test(&self, p: EPoint, screen_rect: Rect, tile_grid: &impl TileGridView,
        egg: Option<Egg>) -> Vec<(Handle, Hit)>
    {
        let mut r = Vec::new();
        let hex_rect = Self::get_render_hex_rect(screen_rect, tile_grid);
        for y in (hex_rect.top..hex_rect.bottom).rev() {
            for x in hex_rect.left..hex_rect.right {
                let pos = EPoint {
                    elevation: p.elevation,
                    point: Point::new(x, y),
                };
                for &objh in self.at(pos).iter().rev() {
                    let obj = self.get(objh).borrow();

                    let mut hit = if let Some(hit) = obj.hit_test(p.point, &self.frm_db, tile_grid) {
                        hit
                    } else {
                        continue;
                    };

                    if let Some(egg) = egg {
                        if self.is_egg_hit(p.point, &*obj, egg, tile_grid) {
                            hit.with_egg = true;
                        }
                    }

                    r.push((objh, hit));
                }
            }
        }
        r
    }

    pub fn distance(&self, from: Handle, to: Handle) -> Option<u32> {
        self.get(from).borrow().distance(&self.get(to).borrow())
    }

    // obj_intersects_with()
    #[must_use]
    fn is_egg_hit(&self, p: Point, obj: &Object, egg: Egg, tile_grid: &impl TileGridView) -> bool {
        if_chain! {
            if let Some(obj_pos) = obj.pos;
            let obj_pos = obj_pos.point;
            if let Some(pid) = obj.pid.proto_id();
            if pid.kind() == EntityKind::Wall || pid.kind() == EntityKind::Scenery;
            then {
                if !egg.hit_test(p, tile_grid, &self.frm_db) {
                    return false;
                }

                let proto = self.proto_db.proto(pid).unwrap();
                let masked = if proto.flags_ext.intersects(
                    FlagExt::WallEastOrWest | FlagExt::WallWestCorner)
                {
                    hex::is_in_front_of(obj_pos, egg.pos)
                } else if proto.flags_ext.contains(FlagExt::WallNorthCorner) {
                    hex::is_in_front_of(obj_pos, egg.pos) ||
                        hex::is_to_right_of(obj_pos, egg.pos)
                } else if proto.flags_ext.contains(FlagExt::WallSouthCorner) {
                    hex::is_in_front_of(obj_pos, egg.pos) &&
                        hex::is_to_right_of(obj_pos, egg.pos)
                } else {
                    hex::is_to_right_of(obj_pos, egg.pos)
                };
                masked
            } else {
                false
            }
        }
    }

    fn get_render_hex_rect(screen_rect: Rect, tile_grid: &impl TileGridView) -> Rect {
        tile_grid.from_screen_rect(Rect {
            left: -320,
            top: -190,
            right: screen_rect.width() + 320,
            bottom: screen_rect.height() + 190
        })
    }

    fn render0(&self, canvas: &mut Canvas, elevation: u32,
            screen_rect: Rect, tile_grid: &impl TileGridView, egg: Option<&Egg>,
            get_light: impl Fn(Option<EPoint>) -> u32,
            flat: bool) {
        let hex_rect = Self::get_render_hex_rect(screen_rect, tile_grid);
        for y in hex_rect.top..hex_rect.bottom {
            for x in (hex_rect.left..hex_rect.right).rev() {
                let pos = EPoint {
                    elevation,
                    point: Point::new(x, y),
                };
                for &objh in self.at(pos) {
                    let mut obj = self.get(objh).borrow_mut();
                    if flat && !obj.flags.contains(Flag::Flat) {
                        break;
                    } else if !flat && obj.flags.contains(Flag::Flat) {
                        continue;
                    }
                    let light = get_light(obj.pos);
                    assert!(light <= 0x10000);
                    obj.render(canvas, light, &self.frm_db, &self.proto_db, tile_grid, egg);
                }
            }
        }
    }

    fn at_mut(&mut self, pos: EPoint) -> &mut Vec<Handle> {
        self.by_pos[pos.elevation as usize]
            .get_mut(pos.point.x as usize, pos.point.y as usize)
            .unwrap()
    }

    fn cmp_objs(&self, o1: &Object, o2: &Object) -> cmp::Ordering {
        assert_eq!(o1.pos.unwrap().elevation, o2.pos.unwrap().elevation);

        // By flatness, flat first.
        let flat = o1.flags.contains(Flag::Flat);
        let other_flat = o2.flags.contains(Flag::Flat);
        if flat != other_flat {
            return if flat {
                cmp::Ordering::Less
            } else {
                cmp::Ordering::Greater
            };
        }


        let shift = o1.screen_shift + o1.do_with_frame(&self.frm_db, |frm| frm.shift);
        let other_shift = o2.screen_shift + o2.do_with_frame(&self.frm_db, |frm| frm.shift);

        // By shift_y, less first.
        if shift.y < other_shift.y {
            return cmp::Ordering::Less;
        }
        if shift.y > other_shift.y {
            return cmp::Ordering::Greater;
        }

        // By shift_x, less first.
        shift.x.cmp(&other_shift.x)
    }

    fn insert_into_tile_grid(&mut self, h: Handle, pos: Option<EPoint>, reset_screen_shift: bool) {
        if let Some(pos) = pos {
            {
                let mut obj = self.get(h).borrow_mut();
                obj.pos = Some(pos);
                if reset_screen_shift {
                    obj.screen_shift = Point::new(0, 0);
                }
            }

            let i = {
                let list = self.at(pos);
                let obj = self.get(h).borrow();
                match list.binary_search_by(|&h| {
                    let o = self.get(h).borrow();
                    self.cmp_objs(&o, &obj)
                }) {
                    Ok(mut i) =>  {
                        // Append to the current group of equal objects.
                        while i < list.len()
                            && self.cmp_objs(&obj, &self.get(list[i]).borrow()) == cmp::Ordering::Equal
                        {
                            i += 1;
                        }
                        i
                    }
                    Err(i) => i,
                }
            };
            self.at_mut(pos).insert(i, h);
        } else {
            self.detached.push(h);
        }
    }

    fn remove_from_tile_grid(&mut self, h: Handle) -> Option<EPoint> {
        let old_pos = mem::replace(&mut self.get(h).borrow_mut().pos, None);
        let list = if let Some(old_pos) = old_pos {
            self.at_mut(old_pos)
        } else {
            &mut self.detached
        };
        // TODO maybe use binary_search for detaching.
        list.retain(|&hh| hh != h);
        old_pos
    }
}

#[derive(Debug)]
pub enum SubObject {
    None,
    Critter(Critter),
}

impl SubObject {
    pub fn critter(&self) -> Option<&Critter> {
        if let SubObject::Critter(v) = self {
            Some(v)
        } else {
            None
        }
    }
}

#[derive(Debug, Default)]
pub struct Critter {
    pub health: i32,
    pub radiation: i32,
    pub poison: i32,
    pub combat: CritterCombat,
}

impl Critter {
    // critter_is_active()
    pub fn is_active(&self) -> bool {
        !self.combat.damage_flags.intersects(
            DamageFlag::LoseTurn |
            DamageFlag::Dead |
            DamageFlag::KnockedOut)
    }

    // critter_is_dead()
    pub fn is_dead(&self) -> bool {
        self.combat.damage_flags.contains(DamageFlag::Dead)
        // TODO
//        if ( stat_level_(result, STAT_current_hp) <= 0 )
//      return 1;
    }
}

#[derive(Debug)]
pub struct CritterCombat {
    pub damage_flags: BitFlags<DamageFlag>,
}

impl Default for CritterCombat {
    fn default() -> Self {
        Self {
            damage_flags: BitFlags::empty(),
        }
    }
}

#[derive(Clone, Copy, Debug, EnumFlags, Primitive)]
#[repr(u32)]
pub enum DamageFlag {
  KnockedOut = 0x1,
  KnockedDown = 0x2,
  CripLegLeft = 0x4,
  CripLegRight = 0x8,
  CripArmLeft = 0x10,
  CripArmRight = 0x20,
  Blind = 0x40,
  Dead = 0x80,
  Hit = 0x100,
  Critical = 0x200,
  OnFire = 0x400,
  Bypass = 0x800,
  Explode = 0x1000,
  Destroy = 0x2000,
  Drop = 0x4000,
  LoseTurn = 0x8000,
  HitSelf = 0x10000,
  LoseAmmo = 0x20000,
  Dud = 0x40000,
  HurtSelf = 0x80000,
  RandomHit = 0x100000,
  CripRandom = 0x200000,
  Backwash = 0x400000,
  PerformReverse = 0x800000,
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::graphics::geometry::hex::View;

    #[test]
    fn bounds() {
        let screen_shift = Point::new(10, 20);
        let base = Point::new(2384, 468) + screen_shift;

        let mut obj = Object::new(FrameId::BLANK, ObjectProtoId::None, Some((0, (55, 66)).into()));
        obj.screen_shift = screen_shift;
        assert_eq!(obj.bounds0(Point::new(-1, 3), Point::new(29, 63), &View::default()),
            Rect::with_points(Point::new(1, -51), Point::new(30, 12))
                .translate(base));
    }
}