---
name: amap-lbs-chat-skill
description: 高德地图综合服务，支持地理编码、逆地理编码、POI 搜索、周边搜索，以及步行、驾车和公交路径规划。
version: 2.0.0
metadata:
  openclaw:
    requires:
      bins:
        - node
    homepage: https://lbs.amap.com/api/webservice/summary
    install:
      - kind: node
        package: axios
        bins: []
---

# 高德地图综合服务 Skill

用于中国大陆的地址与坐标转换、地点和附近店铺搜索，以及路线规划。坐标按高德国内 Web Service 约定使用 `经度,纬度`（GCJ-02）。

## 配置

高德 Web Service Key 从 `AMAP_WEBSERVICE_KEY` 环境变量读取。

不要在调用脚本前预先检查环境变量。正常运行脚本；只有当脚本返回环境变量缺失、Key 为空、Key 无效或权限不足等错误时，才提醒用户设置或检查 `AMAP_WEBSERVICE_KEY` 环境变量。不要要求用户在对话中发送 Key，也不要在回复、日志或命令输出中展示 Key。

## 脚本调用约定

- 本文列出的 `scripts/*.js` 均为已确认的可执行脚本路径，不需要再读取脚本源码确认。
- 相对路径以本 `SKILL.md` 所在目录为基准，使用 Node.js 执行对应脚本。
- 命令示例中的每个 `--名称=值` 都是独立参数。代码块中的返回示意只说明输出结构，不代表实际执行结果。

## 使用原则

- 只在用户询问地点、附近店铺、地址坐标或路线时使用。
- 路线脚本已经裁掉 `polyline`、TMC 几何点等高 token 数据，不要绕过脚本打印原始 API JSON。
- POI 脚本默认最多返回 10 条，路线默认最多返回 10 个主要步骤。需要更多时再分页/提高上限。
- 地点明确且无明显歧义时调低 `--limit`；关键词宽泛或仅使用 `--types` 时调高 `--limit`。
- POI 有 `location` 时用周边搜索；无坐标但关键词含位置时（如“南京西路 咖啡店”）直接关键词搜索。只有要求精确半径、最近距离或路线坐标时才先用 `geocode.js`；地点有歧义时结合城市信息，必要时让用户确认。
- 搜索无结果时，判断地址是否可能不准确或不完整；若能较有把握地推断出正确地址，可修正后重新搜索，否则如实告诉用户未搜索到结果及可能原因。
- 用户提供 WGS-84/GPS 或百度 BD-09 坐标时，不要直接当作高德 GCJ-02 坐标使用；当前技能不支持坐标系转换，应说明限制并请用户提供高德坐标。
- 对“安静、适合约会”等主观要求，可用具体关键词召回后结合返回字段筛选。

## 1. 地理编码

地点/地址 → 坐标：

```bash
node scripts/geocode.js --address="上海静安寺" --city="上海"
```

返回为紧凑 JSON，最多 3 个候选。以下仅为字段结构示意，不是实际查询结果：

```json
{"matches":[{"formatted_address":"<地址>","location":"<经度,纬度>","level":"<匹配级别>"}]}
```

## 2. 逆地理编码

坐标 → 地址：

```bash
node scripts/reverse-geocode.js --location="121.445000,31.223000"
```

输入坐标必须是高德坐标系下的 `经度,纬度`。返回为紧凑 JSON，包含格式化地址以及可用的省、市、区县、乡镇、街道、门牌号和行政区划代码。脚本只请求基础地址信息，不返回附近 POI、道路或路口列表。

## 3. POI / 店铺搜索

### 文字地点关键词搜索（无需坐标）

```bash
node scripts/poi-search.js --keywords="南京西路 咖啡店" --city="上海" --limit=10
```

### 指定坐标附近搜索

```bash
node scripts/poi-search.js --keywords="咖啡" --location="121.445000,31.223000" --radius=1200 --limit=10
```

可选参数：

- `--keywords=`：地点名或类别关键词；和 `--types` 至少提供一个。
- `--types=`：按大类限定结果，可用 `|` 分隔。常用大类：餐饮 `050000`、购物 `060000`、生活服务 `070000`、体育休闲 `080000`、医疗 `090000`、住宿 `100000`、景点 `110000`、交通设施 `150000`；大类会包含所属中类和小类，普通搜索优先使用 `--keywords`。
- `--city=`：城市名/citycode/adcode。
- `--cityLimit=true`：严格限定城市。
- `--location=lng,lat`：提供后自动使用周边搜索。
- `--radius=`：周边半径米，最大 50000。
- `--sort=distance|weight`：周边搜索按距离或综合权重排序。
- `--page=`：页码。
- `--limit=`：1-25，默认 10。

返回字段经过裁剪，可能包括：

- `id / name / address / district / type / location / amap_url / distance_m`
- `rating`
- `cost_per_person`
- `open_today / open_week`
- `tags / business_area / parking_type`

这些商业字段并非所有 POI 都有；缺失时不要臆测。

`amap_url` 是使用 POI ID（缺失时使用坐标和名称）生成的高德官方网页版地点链接，不包含 Web Service Key。回复用户时可以将地点名称链接到该地址。

## 4. 路线规划

输入坐标必须是 `经度,纬度`。

通用调用：

```bash
node scripts/route-planning.js --type=walking --origin="121.445,31.223" --destination="121.455,31.228"
```

`--type` 支持 `walking`、`driving`、`transfer`。

可选参数：

- 公交 `transfer` 必须提供起点城市 `--city=`；跨城时还必须提供终点城市 `--cityd=`。
- 驾车 `driving` 可用 `--waypoints="lng,lat;lng,lat"` 添加途经点。
- `--strategy=`：驾车支持 0-20，默认 10；常用值为 10 默认、12 躲避拥堵、13 不走高速、14 避免收费、19 高速优先、20 高速优先并躲避拥堵。公交支持 0 最快捷、1 最经济、2 最少换乘、3 最少步行、5 不乘地铁，默认 0。
- `--nightflag=true|false`：公交是否计算夜班车，默认 `false`。
- `--maxSteps=`：控制主要步骤或公交段数量，范围 3-20，默认 10。

路线输出为紧凑 JSON，包含：

- 预计时间；步行和驾车还包含总距离
- 返回方案数量，但只详细输出第一套方案
- 驾车时可包含过路费、红绿灯数和限行结果（`restriction`：0 表示已规避或不限行，1 表示有限行且无法规避）
- 最多约 10 个主要路线步骤（`instruction / road / distance_m / duration_s / action`）
- 公交时输出精简换乘段、同段备选线路、地铁出入口和铁路段；发生截断时返回遗漏段数
- `amap_url`：高德官方网页版路线链接，不包含 Web Service Key。网页会按相同起终点和出行方式重新规划，不保证与接口返回的路线步骤完全一致；官方网页 URI 最多支持一个驾车途经点，存在多个途经点时链接只包含起终点。

**永远不要要求或输出 polyline、完整坐标轨迹或原始路线响应。** 用户只是问“怎么走”时，将路线 JSON 概括成自然语言，并可附上 `amap_url` 供用户打开地图查看。

## 常见工作流

### “静安寺附近有什么适合聊天的咖啡店？”

1. `poi-search.js --keywords="静安寺 咖啡店" --city="上海"`
2. 根据评分、人均、营业时间、标签以及用户偏好进行解释和筛选；若用户要求精确距离，再改用地理编码和周边搜索。

### “虹桥机场周边有什么美食？”

1. `poi-search.js --location="121.339775,31.196029" --types="050000" --limit=25`
2. 使用虹桥机场坐标进行周边搜索，不传 `--keywords`，通过餐饮大类召回更多候选，再结合距离、评分、人均和营业时间筛选。

### “从静安寺到虹桥机场怎么走？”

1. 分别地理编码起点、终点；若候选歧义先确认。
2. 根据用户出行方式调用 `route-planning.js`。
3. 用总时间、总距离和主要步骤回复；合适时可以加上高德地图WEB端URL链接。
