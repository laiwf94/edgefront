vcl 4.1;

# The built-in VCL does the rest: it caches what the origin's Cache-Control
# allows, for as long as it allows, and collapses concurrent misses.
backend origin {
    .host = "{{origin_host}}";
    .port = "{{origin_port}}";
}
