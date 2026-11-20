# 图像与 PDF 读取

`core.image.v1` 与 `core.pdf.v1` 消费 Storage 返回的原始 bytes，保留既有 Resolver 身份和 solved 字段。格式解析在阻塞工作线程执行。普通图读取保留作者保存的 pointer，媒体解算按实际字节判断。

图像使用 Rust image decoder 检查 PNG、JPEG、GIF、WebP、BMP、TIFF、ICO、PNM 和 TGA，返回格式、尺寸、字节大小及检测 MIME。PNG/APNG、GIF 和 WebP 按 decoder 检查帧，最多检查一万帧。TIFF 的 frame_count 当前为空，后续需补充多页检查。默认 text 返回能力错误，lexical text 返回媒体事实，label 为 image。

PDF 使用 lopdf 解析对象、页树和元数据。加密文档的 metadata/page_count 为空，text 也为空。文本层最多提取两千页、一百万 Unicode 字符。默认 text 返回文本层，lexical text 同时包含结构元数据，label 优先 title。解码库对复杂字体和损坏文档的容错与 Python pypdf 可能不同，尚需更多真实文档互操作证据。

图像 lexical 读取仅在 `materialize_missing=true` 且部署配置 `core.resolver.image` 存在时尝试 OCR。该配置使用 `core.resolver.image.config.v1`，要求整数 `text_model`。模型接收原始图像字节和既有传输 URL，只转录可见文字；非空输出保存为 `core.text.v1` 子 Block，以 `text` 关系连接原图。lexical 返回值仍为媒体事实，默认 text 仍返回能力错误。关闭物化、缺少配置或已有子图时不会调用模型；空输出和模型故障不会生成替代文字，也不会妨碍事实读取。

共用 `knowledge::materialization` 模块向音频和视频提供相同的文本侧车写入接口，以及严格 typed 配置读取。`core.resolver.audio.config.v1` 要求 `transcript_model`；`core.resolver.video.config.v1` 要求 `transcript_model` 和 `text_model`，配置 key 分别为 `core.resolver.audio` 与 `core.resolver.video`。模型请求在图事务外执行，写入事务锁定父 Block 并复核既有同 role 关系，然后原子提交文字和关系，以避免并发调用重复生成子图。

一次性 loopback AI 与原生 PostgreSQL 验收已覆盖 image 的配置选择、物化开关、精确图像字节、文字侧车、并发复核、已有侧车复用、provider 故障与空输出；这些验收未新增自动化测试套件。
