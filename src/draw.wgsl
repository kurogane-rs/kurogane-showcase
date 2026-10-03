// Draws each star as a small glowing disc on one triangle, its colour from
// its speed.

struct Star {
    pos: vec4<f32>,
    vel: vec4<f32>,
};

struct Draw {
    view_proj: mat4x4<f32>,
    viewport: vec2<f32>,
    size: f32,
    hue: f32,
};

@group(0) @binding(0) var<uniform> draw: Draw;
@group(0) @binding(1) var<storage, read> sky: array<Star>;

struct Fragment {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec3<f32>,
};

fn hsv(h: f32, s: f32, v: f32) -> vec3<f32> {
    let k = vec3<f32>(1.0, 2.0 / 3.0, 1.0 / 3.0);
    let p = abs(fract(vec3<f32>(h) + k) * 6.0 - vec3<f32>(3.0));
    return v * mix(vec3<f32>(1.0), clamp(p - vec3<f32>(1.0), vec3<f32>(0.0), vec3<f32>(1.0)), s);
}

@vertex
fn vs(@builtin(vertex_index) v: u32) -> Fragment {
    // The triangle around the unit circle the glow fills: half the vertices
    // and triangles of a quad, and its corners outside the circle add nothing
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 2.0), vec2<f32>(-1.7320508, -1.0), vec2<f32>(1.7320508, -1.0),
    );
    let star = sky[v / 3u];
    let corner = corners[v % 3u];

    var clip = draw.view_proj * vec4<f32>(star.pos.xyz, 1.0);
    // A glow `size` pixels across, whatever the distance
    clip = vec4<f32>(clip.xy + corner * draw.size / draw.viewport * clip.w, clip.zw);

    // Slow stars take the galaxy's colour, fast ones burn hot
    let heat = clamp(length(star.vel.xyz) / 1.7, 0.0, 1.0);
    let cold = hsv(draw.hue, 0.78, 0.55);
    let hot = hsv(fract(draw.hue + 0.42), 0.5, 1.0);
    let color = mix(cold, hot, heat * heat) + vec3<f32>(0.25) * pow(heat, 6.0);

    var out: Fragment;
    out.clip = clip;
    out.uv = corner;
    out.color = color;
    return out;
}

@fragment
fn fs(in: Fragment) -> @location(0) vec4<f32> {
    let falloff = max(0.0, 1.0 - dot(in.uv, in.uv));
    let a = falloff * falloff * 0.22;
    return vec4<f32>(in.color * a, a);
}
