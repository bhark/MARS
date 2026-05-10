-- osm2pgsql flex output mapping to the e2e_source schema.
-- Produces: land, water, settlements, roads, buildings, waterways
-- with id, geom, and the documented attribute columns.
--
-- Tables are projected to EPSG:25832 (ETRS89 / UTM zone 32N) on load,
-- so downstream consumers see geometry in the target SRID without a
-- post-load reprojection step. flex output does not create indexes
-- implicitly, so each table declares an explicit GIST on geom.

local SRID = 25832
local SCHEMA = 'e2e_source'

local land = osm2pgsql.define_table{
    name = 'land',
    schema = SCHEMA,
    ids = { type = 'any', id_column = 'id', type_column = 'osm_type' },
    columns = {
        { column = 'geom', type = 'geometry', projection = SRID, not_null = true },
    },
    indexes = {
        { column = 'geom', method = 'gist' },
    },
}

local water = osm2pgsql.define_table{
    name = 'water',
    schema = SCHEMA,
    ids = { type = 'any', id_column = 'id', type_column = 'osm_type' },
    columns = {
        { column = 'geom', type = 'geometry', projection = SRID, not_null = true },
    },
    indexes = {
        { column = 'geom', method = 'gist' },
    },
}

local settlements = osm2pgsql.define_table{
    name = 'settlements',
    schema = SCHEMA,
    ids = { type = 'any', id_column = 'id', type_column = 'osm_type' },
    columns = {
        { column = 'geom', type = 'geometry', projection = SRID, not_null = true },
    },
    indexes = {
        { column = 'geom', method = 'gist' },
    },
}

local roads = osm2pgsql.define_table{
    name = 'roads',
    schema = SCHEMA,
    ids = { type = 'way', id_column = 'id' },
    columns = {
        { column = 'geom', type = 'linestring', projection = SRID, not_null = true },
        { column = 'kind', type = 'text' },
    },
    indexes = {
        { column = 'geom', method = 'gist' },
    },
}

local buildings = osm2pgsql.define_table{
    name = 'buildings',
    schema = SCHEMA,
    ids = { type = 'way', id_column = 'id' },
    columns = {
        { column = 'geom', type = 'geometry', projection = SRID, not_null = true },
        { column = 'kind', type = 'text' },
        { column = 'status', type = 'text' },
    },
    indexes = {
        { column = 'geom', method = 'gist' },
    },
}

local waterways = osm2pgsql.define_table{
    name = 'waterways',
    schema = SCHEMA,
    ids = { type = 'way', id_column = 'id' },
    columns = {
        { column = 'geom', type = 'linestring', projection = SRID, not_null = true },
        { column = 'width_class', type = 'text' },
    },
    indexes = {
        { column = 'geom', method = 'gist' },
    },
}

local function is_area(tags)
    return tags.area ~= 'no' and (
        tags.natural or tags.landuse or tags.waterway == 'riverbank'
    )
end

function osm2pgsql.process_way(object)
    local tags = object.tags
    if not tags then return end

    -- buildings
    if tags.building then
        local geom = object:as_polygon()
        if geom then
            local status = 'default'
            if tags.temporary == 'yes' or tags.building == 'temporary' then
                status = 'temporary'
            end
            buildings:insert{
                geom = geom,
                kind = tags.building,
                status = status,
            }
        end
        return
    end

    -- roads
    if tags.highway then
        local geom = object:as_linestring()
        if geom then
            local kind = 'minor'
            local major = { motorway = true, trunk = true, primary = true, secondary = true }
            if major[tags.highway] then
                kind = 'major'
            end
            roads:insert{
                geom = geom,
                kind = kind,
            }
        end
        return
    end

    -- waterways (linear)
    if tags.waterway and tags.waterway ~= 'riverbank' then
        local geom = object:as_linestring()
        if geom then
            local width_class = 'narrow'
            if tags.waterway == 'river' or tags.waterway == 'canal' then
                width_class = 'wide'
            end
            waterways:insert{
                geom = geom,
                width_class = width_class,
            }
        end
        return
    end

    -- area features: land, water, settlements
    if is_area(tags) then
        local geom = object:as_polygon()
        if not geom then return end

        if tags.natural == 'water' or tags.waterway == 'riverbank' then
            water:insert{ geom = geom }
            return
        end

        if tags.place or tags.landuse == 'residential' then
            settlements:insert{ geom = geom }
            return
        end

        -- everything else is land
        land:insert{ geom = geom }
    end
end

function osm2pgsql.process_relation(object)
    local tags = object.tags
    if not tags then return end

    local relation_type = tags.type
    if relation_type == 'multipolygon' or relation_type == 'boundary' then
        local geom = object:as_multipolygon()
        if not geom then return end

        if tags.natural == 'water' then
            water:insert{ geom = geom }
            return
        end

        if tags.place or tags.landuse == 'residential' then
            settlements:insert{ geom = geom }
            return
        end

        -- default to land for area relations
        land:insert{ geom = geom }
    end
end
