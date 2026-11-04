# Shutoko Revival Project (SRP) server

A freeroam server on the SRP highway with AI traffic. The files here are a
working starting point: `server_cfg.ini`, `entry_list.ini` (40 player slots,
40 traffic slots) and `extra_cfg.yml` (AI traffic, AFK and ping kicks).

## What you need

- **The track:** SRP 0.9.4 PTB1, folder `shutoko_revival_project_094_ptb1`,
  from the SRP team. The configs use its `main_layout` (170 pit boxes), whose
  `ai/fast_lane.aip` is the traffic spline.
- **The cars:** the SRP car pack. The sample entry list uses six player cars
  and six `traffic_*` cars from pack 3.6.
- **Players** need the same track version and cars, and Custom Shaders Patch;
  joining through Content Manager is easiest.

## Set up

1. Lay out the content root the server reads with `--content`:

   ```
   srp-server/
     acserver-rs
     cfg/        server_cfg.ini, entry_list.ini, extra_cfg.yml from here
     system/     from the release bundle or your AC install
     content/
       tracks/shutoko_revival_project_094_ptb1/
       cars/<every model in CARS=>/
   ```

   The server checks each car and the track against `content/` at startup,
   and uses the same files for checksums.
2. Set `ADMIN_PASSWORD` and `NAME` in `server_cfg.ini`.
3. Start it from that folder:

   ```sh
   ./acserver-rs --cfg cfg --content .
   ```

   The log should show `AI splines loaded points=… junctions=174`.
4. Open TCP and UDP 9600 and TCP 8081 (README "Open the ports").

## Changing it

- **More players or cars:** add `[CAR_n]` slots to `entry_list.ini`, list any
  new model in `CARS=`, put its folder in `content/cars/`, and raise
  `MAX_CLIENTS` to the number of slots (at most 170 on `main_layout`).
- **More or less traffic:** `AiPerPlayerTargetCount` and `MaxAiTargetCount`
  set how many AI cars there are; more `AI=fixed` slots let more of them be
  drawn at once. `MaxSpeedKph` and `TrafficDensity` set their pace and density.
  Every `AiParams` key is AssettoServer's (README "AI traffic").
- **Time of day:** `SUN_ANGLE` (-8 is dusk) and `TIME_OF_DAY_MULT`.
- **Content downloads** are off: leave `EnableContentDownloads` off unless you
  may redistribute the track and cars.
