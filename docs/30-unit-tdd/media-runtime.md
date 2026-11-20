# 图像与 PDF 读取

`core.image.v1` 与 `core.pdf.v1` 消费 Storage 返回的原始 bytes，保留既有 Resolver 身份和 solved 字段。格式解析在阻塞工作线程执行。普通图读取保留作者保存的 pointer，媒体解算按实际字节判断。

图像使用 Rust image decoder 检查 PNG、JPEG、GIF、WebP、BMP、TIFF、ICO、PNM 和 TGA，返回格式、尺寸、字节大小及检测 MIME。PNG/APNG、GIF 和 WebP 按 decoder 检查帧，最多检查一万帧。TIFF 的 frame_count 当前为空，后续需补充多页检查。默认 text 返回能力错误，lexical text 返回媒体事实，label 为 image。

PDF 使用 lopdf 解析对象、页树和元数据。加密文档的 metadata/page_count 为空，text 也为空。文本层最多提取两千页、一百万 Unicode 字符。默认 text 返回文本层，lexical text 同时包含结构元数据，label 优先 title。解码库对复杂字体和损坏文档的容错与 Python pypdf 可能不同，尚需更多真实文档互操作证据。

当前图像 lexical 读取尚未实现 Python 的配置驱动 OCR text 子图物化。该派生行为需要 `core.resolver.image` 的模型选择、调用 AI 后复核已有 text 关系，以及同一图事务写入。媒体读取不能以调用模型或生成替代内容作为字节检查的前置条件。
