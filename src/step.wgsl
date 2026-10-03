// The galaxy has stars orbiting a central mass. `step` updates them each frame;
// `seed` creates their initial positions.

struct Star {
    pos: vec4<f32>,
    vel: vec4<f32>,
};

struct Sim {
    dt: f32,
    gravity: f32,
    swirl: f32,
    turbulence: f32,
    time: f32,
    burst: f32,
    count: u32,
    seed: u32,
};

const BINS: u32 = 64u;
const RADIUS_RANGE: f32 = 6.0;
const SPEED_RANGE: f32 = 2.5;

@group(0) @binding(0) var<uniform> sim: Sim;
@group(0) @binding(1) var<storage, read_write> stars: array<Star>;
@group(0) @binding(2) var<storage, read_write> bins: array<atomic<u32>>;

var<workgroup> local_bins: array<atomic<u32>, 128>;

fn hash(n: u32) -> u32 {
    var x = n;
    x ^= x >> 16u;
    x *= 0x7feb352du;
    x ^= x >> 15u;
    x *= 0x846ca68bu;
    x ^= x >> 16u;
    return x;
}

fn rand(n: u32) -> f32 {
    return f32(hash(n)) / 4294967295.0;
}

// A star on the disc between two radii, on a near-circular orbit, placed on
// one of two spiral arms
fn spawn(i: u32, salt: u32, r_min: f32, r_max: f32) -> Star {
    let k = hash(i ^ (salt * 0x9e3779b9u));
    let a = rand(k);
    let b = rand(k + 1u);
    let c = rand(k + 2u);
    let d = rand(k + 3u);

    let r = r_min + (r_max - r_min) * a * a;
    let arm = select(0.0, 3.14159265, c > 0.5);
    let theta = arm + log(r + 0.2) * 2.4 + (b - 0.5) * 1.1;
    let thickness = 0.04 + 0.03 * r;
    let pos = vec3<f32>(cos(theta) * r, (d - 0.5) * thickness, sin(theta) * r);

    let speed = sqrt(max(sim.gravity, 0.05) / (r + 0.15));
    let along = vec3<f32>(-sin(theta), 0.0, cos(theta));
    return Star(vec4<f32>(pos, 1.0), vec4<f32>(along * speed, 0.0));
}

// A force field that drifts slowly over time
fn field(p: vec3<f32>, t: f32) -> vec3<f32> {
    return vec3<f32>(
        sin(p.z * 1.7 + t * 0.31) + 0.5 * sin(p.y * 3.1 - t * 0.53),
        0.35 * sin(p.x * 2.3 + p.z * 1.1 + t * 0.4),
        cos(p.x * 1.9 - t * 0.27) + 0.5 * cos(p.y * 2.7 + t * 0.61),
    );
}

@compute @workgroup_size(256)
fn seed(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if (i >= arrayLength(&stars)) {
        return;
    }
    stars[i] = spawn(i, sim.seed, 0.12, 5.6);
}

@compute @workgroup_size(256)
fn step(
    @builtin(global_invocation_id) id: vec3<u32>,
    @builtin(local_invocation_index) lane: u32,
) {
    if (lane < 128u) {
        atomicStore(&local_bins[lane], 0u);
    }
    workgroupBarrier();

    let i = id.x;
    if (i < sim.count) {
        var star = stars[i];
        let r = star.pos.xyz;
        let d2 = dot(r, r) + 0.02;
        let d = sqrt(d2);

        // The central mass, softened so nothing divides by zero
        var acc = -r * (sim.gravity / (d2 * d));
        // A thin disc
        acc.y -= r.y * 2.0;
        // Swirl: a push along the orbit
        acc += cross(vec3<f32>(0.0, 1.0, 0.0), r) * (sim.swirl / (d2 + 0.5));
        acc += field(r, sim.time) * sim.turbulence;
        // Burst: a blast outwards from the centre
        acc += r / d * (sim.burst / (d + 0.3));

        var v = (star.vel.xyz + acc * sim.dt) * (1.0 - 0.015 * sim.dt);
        let next = r + v * sim.dt;
        let nd = length(next);
        if (nd < 0.06 || nd > 9.0) {
            // Swallowed by the centre, or flung out: born again at the rim
            star = spawn(i, sim.seed, 2.6, 5.4);
            v = star.vel.xyz;
        } else {
            star.pos = vec4<f32>(next, 1.0);
            star.vel = vec4<f32>(v, 0.0);
        }
        stars[i] = star;

        let rb = min(u32(length(star.pos.xyz) / RADIUS_RANGE * f32(BINS)), BINS - 1u);
        let sb = min(u32(length(v) / SPEED_RANGE * f32(BINS)), BINS - 1u);
        atomicAdd(&local_bins[rb], 1u);
        atomicAdd(&local_bins[BINS + sb], 1u);
    }

    workgroupBarrier();
    if (lane < 128u) {
        let n = atomicLoad(&local_bins[lane]);
        if (n > 0u) {
            atomicAdd(&bins[lane], n);
        }
    }
}
