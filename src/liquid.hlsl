// Liquid glass, worked out per pixel on the graphics card. Everything is measured in pixels of the
// picture being drawn. The shape is one distance field, so the panel, the neck and the label melt
// into each other, and the lens and the light both follow that shape's curve.

cbuffer Glass : register(b0) {
    float4 box;    // x, y, w, h
    float4 shape;  // corner radius, rim width, blur level, screen scale
    float4 tint;   // colour, strength
    float4 place;  // where this picture's corner is on the wallpaper, then the wallpaper's size
    float4 mouse;  // x, y, how much it leads the light, how far to darken over bright wallpaper
    float4 neck0;  // curve points 0 and 1
    float4 neck1;  // curve points 2 and 3
    float4 neck2;  // on, reach, half width at the ends, half width in the middle
    float4 pill;   // the label the neck comes out of
    float4 look;   // 0 liquid, 1 aurora, 2 neon, 3 clear; seconds; how strongly this piece glows
    float4 glow;   // this section's colour
    float4 fade;   // x: how visible this piece is, 0 to 1; y: casts the wide shadow; z, w: opening effect
};

Texture2D picture : register(t0);
SamplerState smooth_sampler : register(s0);

float4 cover_screen(uint id : SV_VertexID) : SV_Position {
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
}

// A rounded box with continuous corners, the soft squircle curve rather than a circle's arc.
float box_distance(float2 p, float4 b, float r) {
    float2 q = abs(p - b.xy - b.zw / 2) - b.zw / 2 + r;
    float2 o = max(q, 0.0001);
    return pow(pow(o.x, 2.6) + pow(o.y, 2.6), 1 / 2.6) + min(max(q.x, q.y), 0) - r;
}

float melt(float a, float b, float k) {
    float h = max(k - abs(a - b), 0) / k;
    return min(a, b) - h * h * k / 4;
}

float2 curve(float t) {
    float u = 1 - t;
    return u * u * u * neck0.xy + 3 * u * u * t * neck0.zw + 3 * u * t * t * neck1.xy + t * t * t * neck1.zw;
}

float neck_distance(float2 p) {
    float best = 100000;
    float2 a = curve(0);
    for (int i = 1; i <= 16; i++) {
        float t = neck2.y * i / 16.0;
        float2 c = curve(t);
        float2 ab = c - a;
        float s = saturate(dot(p - a, ab) / max(dot(ab, ab), 0.001));
        float along = t - neck2.y * (1 - s) / 16.0;
        float off = pow(2 * along - 1, 4);
        best = min(best, length(p - a - ab * s) - lerp(neck2.w, neck2.z, off));
        a = c;
    }
    return best;
}

// No closer than this to the neck: the curve and its chords stay inside the box round its four
// points, so the distance to that box, less the widest half width, never overshoots.
float neck_at_least(float2 p) {
    float2 lo = min(min(neck0.xy, neck0.zw), min(neck1.xy, neck1.zw));
    float2 hi = max(max(neck0.xy, neck0.zw), max(neck1.xy, neck1.zw));
    return length(max(max(lo - p, p - hi), 0)) - max(neck2.z, neck2.w);
}

float glass_distance(float2 p) {
    float d = box_distance(p, box, shape.x);
    if (neck2.x > 0.5) {
        float k = 14 * shape.w;
        // melt(d, n, k) is exactly d once n >= d + k, so the 16-step curve is only walked near the
        // neck. Walking it for every pixel of a big panel (5 times each) overran the frame.
        if (neck_at_least(p) < d + k) {
            d = melt(d, neck_distance(p), k);
        }
        d = melt(d, box_distance(p, pill, 14 * shape.w), k);
    }
    return d;
}

float3 wallpaper(float2 at, float level) {
    float2 uv = at / place.zw;
    float2 spread = pow(2, level) * 0.5 / place.zw;
    return 0.25 * (picture.SampleLevel(smooth_sampler, uv + spread * float2(-1, -0.4), level).rgb
                 + picture.SampleLevel(smooth_sampler, uv + spread * float2(1, 0.4), level).rgb
                 + picture.SampleLevel(smooth_sampler, uv + spread * float2(-0.4, 1), level).rgb
                 + picture.SampleLevel(smooth_sampler, uv + spread * float2(0.4, -1), level).rgb);
}

float4 glass_core(float4 position : SV_Position) : SV_Target {
    float2 p = position.xy;
    float scale = shape.w;
    float d = glass_distance(p);
    bool neon = abs(look.x - 2) < 0.5;
    bool clear = look.x > 2.5; // Apple's clear glass: sharp in the middle, bent and slightly soft at the rim
    if (d > 0.5) {
        if (neon) {
            // neon: light spilling out of the tube instead of a shadow
            float a = exp(-d / (5 * scale)) * saturate(1 - d / (14 * scale)) * (0.40 + 0.35 * mouse.z) * look.z;
            return float4(glow.rgb * a, a * 0.55);
        }
        if (fade.y > 0.5) {
            // the open panel floats: a faint line hugging its edge, under a wide soft haze dropped
            // below it, as Windows' own flyouts have. It fades to nothing before the picture's edge.
            float edge = max(glass_distance(p - float2(0, 1.5 * scale)), 0);
            float drop = max(glass_distance(p - float2(0, 14 * scale)), 0);
            float reach = saturate(1 - drop / (36 * scale));
            return float4(0, 0, 0, 0.16 * exp(-edge / (2.5 * scale)) + 0.24 * exp(-drop / (13 * scale)) * reach * reach);
        }
        float gap = max(glass_distance(p - float2(0, 5 * scale)), 0);
        return float4(0, 0, 0, 0.30 * exp(-gap / (6 * scale)) * saturate(1 - gap / (13 * scale)));
    }
    float2 n = float2(glass_distance(p + float2(1, 0)) - glass_distance(p - float2(1, 0)),
                      glass_distance(p + float2(0, 1)) - glass_distance(p - float2(0, 1)));
    n /= max(length(n), 0.0001);

    float inside = -d;
    float cover = saturate(0.5 - d);
    if (neon) {
        // neon: dark glass, and a lit tube round the edge in the section's colour
        float3 dark = wallpaper(p + place.xy, 5.0) * 0.16 + float3(0.012, 0.012, 0.03);
        float2 to = mouse.xy - p;
        float2 lit = normalize(lerp(float2(-0.55, -0.83), normalize(to + 0.001), mouse.z));
        float arc_light = pow(max(dot(n, lit), 0), 5) * mouse.z;
        float tube = exp(-inside / (1.3 * scale));
        float haze = exp(-inside / (11 * scale));
        float power = look.z * (0.85 + 0.45 * arc_light);
        float3 lit_colour = dark + glow.rgb * (tube * 1.05 + haze * 0.24) * power
                          + float3(1, 1, 1) * exp(-inside / (0.7 * scale)) * 0.32 * power;
        float a = exp(-max(d, 0) / (5 * scale)) * 0.40 * look.z;
        return float4(saturate(lit_colour) * cover, cover) + float4(glow.rgb * a, a * 0.55) * (1 - cover);
    }
    // the glass swells where the mouse is: a wider, deeper lens right under the pointer
    float swell = mouse.z * exp(-length(mouse.xy - p) / (90 * scale));
    float rim = shape.y * (1 + 0.35 * swell);
    float t = saturate(1 - inside / rim); // 0 inside, 1 at the very edge
    float bend = (clear ? rim * 2.4 * t * t : rim * 1.9 * t * t * t) * (1 + 0.5 * swell);
    float2 at = p + place.xy;
    float aurora = abs(look.x - 1) < 0.5 ? 1.0 : 0.0;
    float level = clear ? shape.z + t * t * 1.5 : shape.z + t * 1.2 + aurora * 1.6;
    // red bends least and blue most, as in a real lens
    float3 colour = float3(wallpaper(at - n * bend * 0.92, level).r,
                           wallpaper(at - n * bend, level).g,
                           wallpaper(at - n * bend * 1.09, level).b);

    float grey = dot(colour, float3(0.3, 0.59, 0.11));
    colour = lerp(grey.xxx, colour, 1.28);

    float3 curtains = 0;
    if (aurora > 0.5) {
        // slow curtains of colour. They are worked out from the screen position, so they run on
        // from one label to the next and into the panel as if it were all one sheet of glass.
        float clock = look.y * 0.16;
        float2 q = at / (360 * scale);
        q += float2(0.55 * sin(q.y * 1.7 + clock * 1.3), 0.42 * sin(q.x * 1.3 - clock));
        float a1 = 0.5 + 0.5 * sin(q.x * 2.1 + clock * 1.1 + 1.5 * sin(q.y * 1.4 - clock * 0.7));
        float a2 = 0.5 + 0.5 * sin(q.y * 2.6 - clock * 0.9 + 1.7 * sin(q.x * 1.1 + clock * 0.6));
        float3 own = picture.SampleLevel(smooth_sampler, at / place.zw, 7).rgb;
        own = saturate(lerp(dot(own, float3(0.3, 0.59, 0.11)).xxx, own, 2.4));
        own = saturate(own / max(max(own.r, max(own.g, own.b)), 0.2));
        // separate bands, because colours mixed evenly turn to mud; the section's own colour,
        // one from the wallpaper, and the green and violet of a real aurora
        float b1 = pow(a1, 3);
        float b2 = pow(a2, 3);
        float b3 = pow(0.5 + 0.5 * sin((q.x + q.y) * 1.6 + clock * 0.8), 3);
        float b4 = pow(0.5 + 0.5 * sin((q.x - q.y) * 1.9 - clock * 0.7 + 2.0), 3);
        curtains = glow.rgb * (0.16 + b1) + own * b2 * 0.7 + float3(0.15, 0.95, 0.7) * b3 * 0.8 + float3(0.62, 0.3, 1.0) * b4 * 0.85;
        colour = colour * 0.30 + curtains * 0.62;
        // never so bright that white words get lost
        float now = dot(colour, float3(0.3, 0.59, 0.11));
        colour *= min(1, 0.50 / max(now, 0.001));
    }

    // keeps text readable: the brighter the wallpaper here, the deeper the glass
    float around = dot(picture.SampleLevel(smooth_sampler, at / place.zw, 6).rgb, float3(0.3, 0.59, 0.11));
    colour *= 1 - mouse.w * smoothstep(0.38, 0.85, max(around, grey * 0.8));
    colour = lerp(colour, tint.rgb, tint.a);
    colour *= 1 - (clear ? 0.05 : 0.16) * t * t;

    // the light comes from the top left until the mouse is near, then from the mouse
    float2 to_mouse = mouse.xy - p;
    float2 light = normalize(lerp(float2(-0.55, -0.83), normalize(to_mouse + 0.001), mouse.z));
    // while a panel pours open, the glint sweeps round its rim
    if (neck2.x > 0.5) {
        float turn = (1 - neck2.y) * 3.2;
        light = float2(light.x * cos(turn) - light.y * sin(turn), light.x * sin(turn) + light.y * cos(turn));
    }
    float facing = dot(n, light);
    float edge = exp(-inside / (1.5 * scale));
    float wide_edge = exp(-inside / (9 * scale));

    // glass reflects its surroundings, not white: the rim takes the wallpaper's own colour from
    // just outside the shape, made vivid, with only a little white in the hottest part
    float3 seen = wallpaper(at + n * shape.y * 2.5, 4.0);
    seen = saturate(lerp(dot(seen, float3(0.3, 0.59, 0.11)).xxx, seen, 1.9));
    seen = saturate(seen / max(max(seen.r, max(seen.g, seen.b)), 0.25));
    if (aurora > 0.5) {
        seen = lerp(seen, saturate(curtains / max(max(curtains.r, max(curtains.g, curtains.b)), 0.25)), 0.8);
    }

    // a short arc that lives where the light is; at rest the rim stays quiet
    float arc = pow(max(facing, 0), 7);
    float awake = 0.5 + 0.5 * mouse.z;
    float3 shine = seen * (arc * edge * 0.75 * awake
                         + pow(max(-facing, 0), 7) * edge * 0.20 * awake
                         + arc * wide_edge * 0.10
                         + edge * 0.10)
                 + float3(1, 1, 1) * arc * arc * edge * 0.30 * awake;
    float near = 170 * scale;
    shine += seen * mouse.z * exp(-dot(to_mouse, to_mouse) / (near * near)) * 0.07;
    colour = saturate(colour + shine);

    return float4(colour * cover, cover) + float4(0, 0, 0, 0.30 * (1 - cover));
}

// Opening effects (fade.w): 2 curtain, a circle spreading from where the neck meets the panel with
// a lit edge; 5 trace, a lit line drawing the outline round from the neck before the glass fills.
float2 stage_origin() {
    return neck2.x > 0.5 ? neck1.zw : box.xy;
}

float curtain_radius() {
    float2 o = stage_origin();
    float far = max(max(length(box.xy - o), length(box.xy + float2(box.z, 0) - o)),
                    max(length(box.xy + float2(0, box.w) - o), length(box.xy + box.zw - o)));
    return fade.z * far;
}

float stage_mask(float2 p) {
    if (abs(fade.w - 2) < 0.5) {
        return saturate((curtain_radius() - length(p - stage_origin())) / (26 * shape.w));
    }
    if (abs(fade.w - 5) < 0.5) {
        return saturate(fade.z - 1);
    }
    return 1;
}

float4 stage_light(float2 p) {
    float scale = shape.w;
    if (abs(fade.w - 2) < 0.5) {
        float cover = saturate(0.5 - glass_distance(p));
        float off = abs(length(p - stage_origin()) - curtain_radius() + 10 * scale);
        float a = exp(-off / (7 * scale)) * cover * 0.55 * saturate(1.1 - fade.z) * 4;
        return float4(saturate(glow.rgb * 0.8 + 0.35) * a, a * 0.8);
    }
    if (abs(fade.w - 5) < 0.5) {
        float d = glass_distance(p);
        float2 c = box.xy + box.zw / 2;
        float2 q = (p - c) / max(box.zw / 2, 1);
        float2 o = (stage_origin() - c) / max(box.zw / 2, 1);
        float round_from_neck = acos(clamp(dot(normalize(q + 0.0001), normalize(o + 0.0001)), -1, 1)) / 3.14159;
        float drawn = saturate((saturate(fade.z) * 1.03 - round_from_neck) / 0.025);
        float lit = exp(-abs(d) / (1.6 * scale)) * drawn * (1 - 0.7 * saturate(fade.z - 1));
        float core = exp(-abs(d) / (0.6 * scale)) * drawn;
        float3 colour = saturate(glow.rgb * 1.3 + 0.15) * lit + core * 0.5;
        return float4(colour, saturate(lit * 0.9 + core * 0.4));
    }
    return 0;
}

float4 glass(float4 position : SV_Position) : SV_Target {
    float4 c = glass_core(position) * stage_mask(position.xy) + stage_light(position.xy);
    return saturate(c) * fade.x;
}

float4 words(float4 position : SV_Position) : SV_Target {
    return picture.Load(int3(position.xy, 0)) * fade.x * stage_mask(position.xy);
}

// Text with each of red, green and blue covered by its own amount, as ClearType asks: t0 holds the
// colour already mixed with that amount, t1 the amounts. The second output tells the blend how much
// of the glass to keep behind each colour.
Texture2D text_cover : register(t1);

struct NamesOut {
    float4 colour : SV_Target0;
    float4 keep : SV_Target1;
};

NamesOut names(float4 position : SV_Position) {
    float3 cover = text_cover.Load(int3(position.xy, 0)).rgb * fade.x * stage_mask(position.xy);
    float most = max(cover.r, max(cover.g, cover.b));
    NamesOut out_;
    out_.colour = float4(picture.Load(int3(position.xy, 0)).rgb * fade.x * stage_mask(position.xy), most);
    out_.keep = float4(cover, most);
    return out_;
}
